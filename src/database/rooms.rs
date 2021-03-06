mod edus;

pub use edus::RoomEdus;
use member::MembershipState;

use crate::{pdu::PduBuilder, utils, Database, Error, PduEvent, Result};
use log::{debug, error, warn};
use regex::Regex;
use ring::digest;
use ruma::{
    api::{client::error::ErrorKind, federation},
    events::{
        ignored_user_list, push_rules,
        room::{create::CreateEventContent, member, message},
        AnyStrippedStateEvent, AnySyncStateEvent, EventType,
    },
    push::{self, Action, Tweak},
    serde::{CanonicalJsonObject, CanonicalJsonValue, Raw},
    state_res::{self, Event, RoomVersion, StateMap},
    uint, EventId, RoomAliasId, RoomId, RoomVersionId, ServerName, UserId,
};
use sled::IVec;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    mem,
    sync::Arc,
};

use super::{admin::AdminCommand, pusher};

/// The unique identifier of each state group.
///
/// This is created when a state group is added to the database by
/// hashing the entire state.
pub type StateHashId = IVec;

#[derive(Clone)]
pub struct Rooms {
    pub edus: edus::RoomEdus,
    pub(super) pduid_pdu: sled::Tree, // PduId = RoomId + Count
    pub(super) eventid_pduid: sled::Tree,
    pub(super) roomid_pduleaves: sled::Tree,
    pub(super) alias_roomid: sled::Tree,
    pub(super) aliasid_alias: sled::Tree, // AliasId = RoomId + Count
    pub(super) publicroomids: sled::Tree,

    pub(super) tokenids: sled::Tree, // TokenId = RoomId + Token + PduId

    /// Participating servers in a room.
    pub(super) roomserverids: sled::Tree, // RoomServerId = RoomId + ServerName
    pub(super) serverroomids: sled::Tree, // ServerRoomId = ServerName + RoomId

    pub(super) userroomid_joined: sled::Tree,
    pub(super) roomuserid_joined: sled::Tree,
    pub(super) roomuseroncejoinedids: sled::Tree,
    pub(super) userroomid_invitestate: sled::Tree, // InviteState = Vec<Raw<Pdu>>
    pub(super) roomuserid_invitecount: sled::Tree, // InviteCount = Count
    pub(super) userroomid_leftstate: sled::Tree,
    pub(super) roomuserid_leftcount: sled::Tree,

    pub(super) userroomid_notificationcount: sled::Tree, // NotifyCount = u64
    pub(super) userroomid_highlightcount: sled::Tree,    // HightlightCount = u64

    /// Remember the current state hash of a room.
    pub(super) roomid_shortstatehash: sled::Tree,
    /// Remember the state hash at events in the past.
    pub(super) shorteventid_shortstatehash: sled::Tree,
    /// StateKey = EventType + StateKey, ShortStateKey = Count
    pub(super) statekey_shortstatekey: sled::Tree,
    pub(super) shorteventid_eventid: sled::Tree,
    /// ShortEventId = Count
    pub(super) eventid_shorteventid: sled::Tree,
    /// ShortEventId = Count
    pub(super) statehash_shortstatehash: sled::Tree,
    /// ShortStateHash = Count
    /// StateId = ShortStateHash + ShortStateKey
    pub(super) stateid_shorteventid: sled::Tree,

    /// RoomId + EventId -> outlier PDU.
    /// Any pdu that has passed the steps 1-8 in the incoming event /federation/send/txn.
    pub(super) eventid_outlierpdu: sled::Tree,

    /// RoomId + EventId -> Parent PDU EventId.
    pub(super) prevevent_parent: sled::Tree,
}

impl Rooms {
    /// Builds a StateMap by iterating over all keys that start
    /// with state_hash, this gives the full state for the given state_hash.
    pub fn state_full_ids(&self, shortstatehash: u64) -> Result<Vec<EventId>> {
        Ok(self
            .stateid_shorteventid
            .scan_prefix(&shortstatehash.to_be_bytes())
            .values()
            .filter_map(|r| r.ok())
            .map(|bytes| self.shorteventid_eventid.get(&bytes).ok().flatten())
            .flatten()
            .map(|bytes| {
                Ok::<_, Error>(
                    EventId::try_from(utils::string_from_bytes(&bytes).map_err(|_| {
                        Error::bad_database("EventID in stateid_shorteventid is invalid unicode.")
                    })?)
                    .map_err(|_| {
                        Error::bad_database("EventId in stateid_shorteventid is invalid.")
                    })?,
                )
            })
            .filter_map(|r| r.ok())
            .collect())
    }

    pub fn state_full(
        &self,
        shortstatehash: u64,
    ) -> Result<BTreeMap<(EventType, String), PduEvent>> {
        Ok(self
            .stateid_shorteventid
            .scan_prefix(shortstatehash.to_be_bytes())
            .values()
            .filter_map(|r| r.ok())
            .map(|bytes| self.shorteventid_eventid.get(&bytes).ok().flatten())
            .flatten()
            .map(|bytes| {
                Ok::<_, Error>(
                    EventId::try_from(utils::string_from_bytes(&bytes).map_err(|_| {
                        Error::bad_database("EventID in stateid_shorteventid is invalid unicode.")
                    })?)
                    .map_err(|_| {
                        Error::bad_database("EventId in stateid_shorteventid is invalid.")
                    })?,
                )
            })
            .filter_map(|r| r.ok())
            .map(|eventid| self.get_pdu(&eventid))
            .filter_map(|r| r.ok().flatten())
            .map(|pdu| {
                Ok::<_, Error>((
                    (
                        pdu.kind.clone(),
                        pdu.state_key
                            .as_ref()
                            .ok_or_else(|| Error::bad_database("State event has no state key."))?
                            .clone(),
                    ),
                    pdu,
                ))
            })
            .filter_map(|r| r.ok())
            .collect())
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn state_get_id(
        &self,
        shortstatehash: u64,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<EventId>> {
        let mut key = event_type.as_ref().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&state_key.as_bytes());

        let shortstatekey = self.statekey_shortstatekey.get(&key)?;

        if let Some(shortstatekey) = shortstatekey {
            let mut stateid = shortstatehash.to_be_bytes().to_vec();
            stateid.extend_from_slice(&shortstatekey);

            Ok(self
                .stateid_shorteventid
                .get(&stateid)?
                .map(|bytes| self.shorteventid_eventid.get(&bytes).ok().flatten())
                .flatten()
                .map(|bytes| {
                    Ok::<_, Error>(
                        EventId::try_from(utils::string_from_bytes(&bytes).map_err(|_| {
                            Error::bad_database(
                                "EventID in stateid_shorteventid is invalid unicode.",
                            )
                        })?)
                        .map_err(|_| {
                            Error::bad_database("EventId in stateid_shorteventid is invalid.")
                        })?,
                    )
                })
                .map(|r| r.ok())
                .flatten())
        } else {
            Ok(None)
        }
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn state_get(
        &self,
        shortstatehash: u64,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<PduEvent>> {
        self.state_get_id(shortstatehash, event_type, state_key)?
            .map_or(Ok(None), |event_id| self.get_pdu(&event_id))
    }

    /// Returns the state hash for this pdu.
    #[tracing::instrument(skip(self))]
    pub fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_shorteventid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |shorteventid| {
                Ok(self.shorteventid_shortstatehash.get(shorteventid)?.map_or(
                    Ok::<_, Error>(None),
                    |bytes| {
                        Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                            Error::bad_database(
                                "Invalid shortstatehash bytes in shorteventid_shortstatehash",
                            )
                        })?))
                    },
                )?)
            })
    }

    /// Returns the last state hash key added to the db for the given room.
    #[tracing::instrument(skip(self))]
    pub fn current_shortstatehash(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.roomid_shortstatehash
            .get(room_id.as_bytes())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid shortstatehash in roomid_shortstatehash")
                })?))
            })
    }

    /// This fetches auth events from the current state.
    pub fn get_auth_events(
        &self,
        room_id: &RoomId,
        kind: &EventType,
        sender: &UserId,
        state_key: Option<&str>,
        content: &serde_json::Value,
    ) -> Result<StateMap<Arc<PduEvent>>> {
        let auth_events = state_res::auth_types_for_event(
            kind,
            sender,
            state_key.map(|s| s.to_string()),
            content.clone(),
        );

        let mut events = StateMap::new();
        for (event_type, state_key) in auth_events {
            if let Some(pdu) = self.room_state_get(room_id, &event_type, &state_key)? {
                events.insert((event_type, state_key), Arc::new(pdu));
            } else {
                // This is okay because when creating a new room some events were not created yet
                debug!(
                    "{:?}: Could not find {} {:?} in state",
                    content, event_type, state_key
                );
            }
        }
        Ok(events)
    }

    /// Generate a new StateHash.
    ///
    /// A unique hash made from hashing all PDU ids of the state joined with 0xff.
    fn calculate_hash(&self, bytes_list: &[&[u8]]) -> StateHashId {
        // We only hash the pdu's event ids, not the whole pdu
        let bytes = bytes_list.join(&0xff);
        let hash = digest::digest(&digest::SHA256, &bytes);
        hash.as_ref().into()
    }

    /// Checks if a room exists.
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        // Look for PDUs in that room.
        Ok(self
            .pduid_pdu
            .get_gt(&prefix)?
            .filter(|(k, _)| k.starts_with(&prefix))
            .is_some())
    }

    /// Force the creation of a new StateHash and insert it into the db.
    ///
    /// Whatever `state` is supplied to `force_state` __is__ the current room state snapshot.
    pub fn force_state(
        &self,
        room_id: &RoomId,
        state: BTreeMap<(EventType, String), EventId>,
        db: &Database,
    ) -> Result<()> {
        let state_hash = self.calculate_hash(
            &state
                .values()
                .map(|event_id| event_id.as_bytes())
                .collect::<Vec<_>>(),
        );

        let (shortstatehash, already_existed) =
            match self.statehash_shortstatehash.get(&state_hash)? {
                Some(shortstatehash) => (
                    utils::u64_from_bytes(&shortstatehash)
                        .map_err(|_| Error::bad_database("Invalid shortstatehash in db."))?,
                    true,
                ),
                None => {
                    let shortstatehash = db.globals.next_count()?;
                    self.statehash_shortstatehash
                        .insert(&state_hash, &shortstatehash.to_be_bytes())?;
                    (shortstatehash, false)
                }
            };

        let new_state = if !already_existed {
            let mut new_state = HashSet::new();

            for ((event_type, state_key), eventid) in state {
                new_state.insert(eventid.clone());

                let mut statekey = event_type.as_ref().as_bytes().to_vec();
                statekey.push(0xff);
                statekey.extend_from_slice(&state_key.as_bytes());

                let shortstatekey = match self.statekey_shortstatekey.get(&statekey)? {
                    Some(shortstatekey) => shortstatekey.to_vec(),
                    None => {
                        let shortstatekey = db.globals.next_count()?;
                        self.statekey_shortstatekey
                            .insert(&statekey, &shortstatekey.to_be_bytes())?;
                        shortstatekey.to_be_bytes().to_vec()
                    }
                };

                let shorteventid = match self.eventid_shorteventid.get(eventid.as_bytes())? {
                    Some(shorteventid) => shorteventid.to_vec(),
                    None => {
                        let shorteventid = db.globals.next_count()?;
                        self.eventid_shorteventid
                            .insert(eventid.as_bytes(), &shorteventid.to_be_bytes())?;
                        self.shorteventid_eventid
                            .insert(&shorteventid.to_be_bytes(), eventid.as_bytes())?;
                        shorteventid.to_be_bytes().to_vec()
                    }
                };

                let mut state_id = shortstatehash.to_be_bytes().to_vec();
                state_id.extend_from_slice(&shortstatekey);

                self.stateid_shorteventid
                    .insert(&state_id, &*shorteventid)?;
            }

            new_state
        } else {
            self.state_full_ids(shortstatehash)?.into_iter().collect()
        };

        let old_state = self
            .current_shortstatehash(&room_id)?
            .map(|s| self.state_full_ids(s))
            .transpose()?
            .map(|vec| vec.into_iter().collect::<HashSet<_>>())
            .unwrap_or_default();

        for event_id in new_state.difference(&old_state) {
            if let Some(pdu) = self.get_pdu_json(event_id)? {
                if pdu.get("type").and_then(|val| val.as_str()) == Some("m.room.member") {
                    if let Ok(pdu) = serde_json::from_value::<PduEvent>(
                        serde_json::to_value(&pdu).expect("CanonicalJsonObj is a valid JsonValue"),
                    ) {
                        if let Some(membership) =
                            pdu.content.get("membership").and_then(|membership| {
                                serde_json::from_value::<member::MembershipState>(
                                    membership.clone(),
                                )
                                .ok()
                            })
                        {
                            if let Some(state_key) = pdu
                                .state_key
                                .and_then(|state_key| UserId::try_from(state_key).ok())
                            {
                                self.update_membership(
                                    room_id,
                                    &state_key,
                                    membership,
                                    &pdu.sender,
                                    None,
                                    db,
                                )?;
                            }
                        }
                    }
                }
            }
        }

        self.roomid_shortstatehash
            .insert(room_id.as_bytes(), &shortstatehash.to_be_bytes())?;

        Ok(())
    }

    /// Returns the full room state.
    #[tracing::instrument(skip(self))]
    pub fn room_state_full(
        &self,
        room_id: &RoomId,
    ) -> Result<BTreeMap<(EventType, String), PduEvent>> {
        if let Some(current_shortstatehash) = self.current_shortstatehash(room_id)? {
            self.state_full(current_shortstatehash)
        } else {
            Ok(BTreeMap::new())
        }
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn room_state_get_id(
        &self,
        room_id: &RoomId,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<EventId>> {
        if let Some(current_shortstatehash) = self.current_shortstatehash(room_id)? {
            self.state_get_id(current_shortstatehash, event_type, state_key)
        } else {
            Ok(None)
        }
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn room_state_get(
        &self,
        room_id: &RoomId,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<PduEvent>> {
        if let Some(current_shortstatehash) = self.current_shortstatehash(room_id)? {
            self.state_get(current_shortstatehash, event_type, state_key)
        } else {
            Ok(None)
        }
    }

    /// Returns the `count` of this pdu's id.
    #[tracing::instrument(skip(self))]
    pub fn pdu_count(&self, pdu_id: &[u8]) -> Result<u64> {
        Ok(
            utils::u64_from_bytes(&pdu_id[pdu_id.len() - mem::size_of::<u64>()..pdu_id.len()])
                .map_err(|_| Error::bad_database("PDU has invalid count bytes."))?,
        )
    }

    /// Returns the `count` of this pdu's id.
    pub fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| self.pdu_count(&pdu_id).map(Some))
    }

    pub fn latest_pdu_count(&self, room_id: &RoomId) -> Result<u64> {
        self.pduid_pdu
            .scan_prefix(room_id.as_bytes())
            .last()
            .map(|b| self.pdu_count(&b?.0))
            .transpose()
            .map(|op| op.unwrap_or_default())
    }

    /// Returns the json of a pdu.
    pub fn get_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or_else::<Result<_>, _, _>(
                || Ok(self.eventid_outlierpdu.get(event_id.as_bytes())?),
                |pduid| {
                    Ok(Some(self.pduid_pdu.get(&pduid)?.ok_or_else(|| {
                        Error::bad_database("Invalid pduid in eventid_pduid.")
                    })?))
                },
            )?
            .map(|pdu| {
                Ok(serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?)
            })
            .transpose()
    }

    /// Returns the pdu's id.
    pub fn get_pdu_id(&self, event_id: &EventId) -> Result<Option<IVec>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| Ok(Some(pdu_id)))
    }

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    pub fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or_else::<Result<_>, _, _>(
                || Ok(None),
                |pduid| {
                    Ok(Some(self.pduid_pdu.get(&pduid)?.ok_or_else(|| {
                        Error::bad_database("Invalid pduid in eventid_pduid.")
                    })?))
                },
            )?
            .map(|pdu| {
                Ok(serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?)
            })
            .transpose()
    }

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    pub fn get_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or_else::<Result<_>, _, _>(
                || Ok(self.eventid_outlierpdu.get(event_id.as_bytes())?),
                |pduid| {
                    Ok(Some(self.pduid_pdu.get(&pduid)?.ok_or_else(|| {
                        Error::bad_database("Invalid pduid in eventid_pduid.")
                    })?))
                },
            )?
            .map(|pdu| {
                Ok(serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?)
            })
            .transpose()
    }

    /// Returns the pdu.
    ///
    /// This does __NOT__ check the outliers `Tree`.
    pub fn get_pdu_from_id(&self, pdu_id: &[u8]) -> Result<Option<PduEvent>> {
        self.pduid_pdu.get(pdu_id)?.map_or(Ok(None), |pdu| {
            Ok(Some(
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
    pub fn get_pdu_json_from_id(&self, pdu_id: &[u8]) -> Result<Option<CanonicalJsonObject>> {
        self.pduid_pdu.get(pdu_id)?.map_or(Ok(None), |pdu| {
            Ok(Some(
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Removes a pdu and creates a new one with the same id.
    fn replace_pdu(&self, pdu_id: &IVec, pdu: &PduEvent) -> Result<()> {
        if self.pduid_pdu.get(&pdu_id)?.is_some() {
            self.pduid_pdu.insert(
                &pdu_id,
                &*serde_json::to_string(pdu).expect("PduEvent::to_string always works"),
            )?;
            Ok(())
        } else {
            Err(Error::BadRequest(
                ErrorKind::NotFound,
                "PDU does not exist.",
            ))
        }
    }

    /// Returns the leaf pdus of a room.
    pub fn get_pdu_leaves(&self, room_id: &RoomId) -> Result<HashSet<EventId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomid_pduleaves
            .scan_prefix(prefix)
            .values()
            .map(|bytes| {
                Ok::<_, Error>(
                    EventId::try_from(utils::string_from_bytes(&bytes?).map_err(|_| {
                        Error::bad_database("EventID in roomid_pduleaves is invalid unicode.")
                    })?)
                    .map_err(|_| Error::bad_database("EventId in roomid_pduleaves is invalid."))?,
                )
            })
            .collect()
    }

    /// Replace the leaves of a room.
    ///
    /// The provided `event_ids` become the new leaves, this allows a room to have multiple
    /// `prev_events`.
    pub fn replace_pdu_leaves(&self, room_id: &RoomId, event_ids: &[EventId]) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        for key in self.roomid_pduleaves.scan_prefix(&prefix).keys() {
            self.roomid_pduleaves.remove(key?)?;
        }

        for event_id in event_ids {
            let mut key = prefix.to_owned();
            key.extend_from_slice(event_id.as_bytes());
            self.roomid_pduleaves.insert(&key, event_id.as_bytes())?;
        }

        Ok(())
    }

    pub fn is_pdu_referenced(&self, pdu: &PduEvent) -> Result<bool> {
        let mut key = pdu.room_id().as_bytes().to_vec();
        key.extend_from_slice(pdu.event_id().as_bytes());
        self.prevevent_parent.contains_key(key).map_err(Into::into)
    }

    /// Returns the pdu from the outlier tree.
    pub fn get_pdu_outlier(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.eventid_outlierpdu
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu| {
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
    }

    /// Append the PDU as an outlier.
    ///
    /// Any event given to this will be processed (state-res) on another thread.
    pub fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) -> Result<()> {
        self.eventid_outlierpdu.insert(
            &event_id.as_bytes(),
            &*serde_json::to_string(&pdu).expect("CanonicalJsonObject is valid string"),
        )?;

        Ok(())
    }

    /// Creates a new persisted data unit and adds it to a room.
    ///
    /// By this point the incoming event should be fully authenticated, no auth happens
    /// in `append_pdu`.
    #[allow(clippy::too_many_arguments)]
    pub fn append_pdu(
        &self,
        pdu: &PduEvent,
        mut pdu_json: CanonicalJsonObject,
        count: u64,
        pdu_id: IVec,
        leaves: &[EventId],
        db: &Database,
    ) -> Result<()> {
        // Make unsigned fields correct. This is not properly documented in the spec, but state
        // events need to have previous content in the unsigned field, so clients can easily
        // interpret things like membership changes
        if let Some(state_key) = &pdu.state_key {
            if let CanonicalJsonValue::Object(unsigned) = pdu_json
                .entry("unsigned".to_owned())
                .or_insert_with(|| CanonicalJsonValue::Object(Default::default()))
            {
                if let Some(shortstatehash) = self.pdu_shortstatehash(&pdu.event_id).unwrap() {
                    if let Some(prev_state) = self
                        .state_get(shortstatehash, &pdu.kind, &state_key)
                        .unwrap()
                    {
                        unsigned.insert(
                            "prev_content".to_owned(),
                            CanonicalJsonValue::Object(
                                utils::to_canonical_object(prev_state.content)
                                    .expect("event is valid, we just created it"),
                            ),
                        );
                    }
                }
            } else {
                error!("Invalid unsigned type in pdu.");
            }
        }

        // We must keep track of all events that have been referenced.
        for leaf in leaves {
            let mut key = pdu.room_id().as_bytes().to_vec();
            key.extend_from_slice(leaf.as_bytes());
            self.prevevent_parent
                .insert(key, pdu.event_id().as_bytes())?;
        }

        self.replace_pdu_leaves(&pdu.room_id, leaves)?;

        // Mark as read first so the sending client doesn't get a notification even if appending
        // fails
        self.edus
            .private_read_set(&pdu.room_id, &pdu.sender, count, &db.globals)?;
        self.reset_notification_counts(&pdu.sender, &pdu.room_id)?;

        self.pduid_pdu.insert(
            &pdu_id,
            &*serde_json::to_string(&pdu_json)
                .expect("CanonicalJsonObject is always a valid String"),
        )?;

        // This also replaces the eventid of any outliers with the correct
        // pduid, removing the place holder.
        self.eventid_pduid
            .insert(pdu.event_id.as_bytes(), &*pdu_id)?;

        // See if the event matches any known pushers
        for user in db
            .users
            .iter()
            .filter_map(|r| r.ok())
            .filter(|user_id| self.is_joined(&user_id, &pdu.room_id).unwrap_or(false))
            .filter(|user_id| !db.users.is_deactivated(user_id).unwrap_or(false))
        {
            // Don't notify the user of their own events
            if user == pdu.sender {
                continue;
            }

            let rules_for_user = db
                .account_data
                .get::<push_rules::PushRulesEvent>(None, &user, EventType::PushRules)?
                .map(|ev| ev.content.global)
                .unwrap_or_else(|| push::Ruleset::server_default(&user));

            let mut highlight = false;
            let mut notify = false;

            for action in pusher::get_actions(&user, &rules_for_user, pdu, db)? {
                match action {
                    Action::DontNotify => notify = false,
                    // TODO: Implement proper support for coalesce
                    Action::Notify | Action::Coalesce => notify = true,
                    Action::SetTweak(Tweak::Highlight(true)) => {
                        highlight = true;
                    }
                    _ => {}
                };
            }

            let mut userroom_id = user.as_bytes().to_vec();
            userroom_id.push(0xff);
            userroom_id.extend_from_slice(pdu.room_id.as_bytes());

            if notify {
                self.userroomid_notificationcount
                    .update_and_fetch(&userroom_id, utils::increment)?
                    .expect("utils::increment will always put in a value");
            }

            if highlight {
                self.userroomid_highlightcount
                    .update_and_fetch(&userroom_id, utils::increment)?
                    .expect("utils::increment will always put in a value");
            }

            for senderkey in db
                .pusher
                .get_pusher_senderkeys(&user)
                .filter_map(|r| r.ok())
            {
                db.sending.send_push_pdu(&*pdu_id, senderkey)?;
            }
        }

        match pdu.kind {
            EventType::RoomRedaction => {
                if let Some(redact_id) = &pdu.redacts {
                    self.redact_pdu(&redact_id, &pdu)?;
                }
            }
            EventType::RoomMember => {
                if let Some(state_key) = &pdu.state_key {
                    // if the state_key fails
                    let target_user_id = UserId::try_from(state_key.clone())
                        .expect("This state_key was previously validated");

                    let membership = serde_json::from_value::<member::MembershipState>(
                        pdu.content
                            .get("membership")
                            .ok_or(Error::BadRequest(
                                ErrorKind::InvalidParam,
                                "Invalid member event content",
                            ))?
                            .clone(),
                    )
                    .map_err(|_| {
                        Error::BadRequest(
                            ErrorKind::InvalidParam,
                            "Invalid membership state content.",
                        )
                    })?;

                    let invite_state = match membership {
                        member::MembershipState::Invite => {
                            let state = self.calculate_invite_state(pdu)?;

                            Some(state)
                        }
                        _ => None,
                    };

                    // Update our membership info, we do this here incase a user is invited
                    // and immediately leaves we need the DB to record the invite event for auth
                    self.update_membership(
                        &pdu.room_id,
                        &target_user_id,
                        membership,
                        &pdu.sender,
                        invite_state,
                        db,
                    )?;
                }
            }
            EventType::RoomMessage => {
                if let Some(body) = pdu.content.get("body").and_then(|b| b.as_str()) {
                    for word in body
                        .split_terminator(|c: char| !c.is_alphanumeric())
                        .map(str::to_lowercase)
                    {
                        let mut key = pdu.room_id.as_bytes().to_vec();
                        key.push(0xff);
                        key.extend_from_slice(word.as_bytes());
                        key.push(0xff);
                        key.extend_from_slice(&pdu_id);
                        self.tokenids.insert(key, &[])?;
                    }

                    if body.starts_with(&format!("@conduit:{}: ", db.globals.server_name()))
                        && self
                            .id_from_alias(
                                &format!("#admins:{}", db.globals.server_name())
                                    .try_into()
                                    .expect("#admins:server_name is a valid room alias"),
                            )?
                            .as_ref()
                            == Some(&pdu.room_id)
                    {
                        let mut lines = body.lines();
                        let command_line = lines.next().expect("each string has at least one line");
                        let body = lines.collect::<Vec<_>>();

                        let mut parts = command_line.split_whitespace().skip(1);
                        if let Some(command) = parts.next() {
                            let args = parts.collect::<Vec<_>>();

                            match command {
                                "register_appservice" => {
                                    if body.len() > 2
                                        && body[0].trim() == "```"
                                        && body.last().unwrap().trim() == "```"
                                    {
                                        let appservice_config = body[1..body.len() - 1].join("\n");
                                        let parsed_config = serde_yaml::from_str::<serde_yaml::Value>(
                                            &appservice_config,
                                        );
                                        match parsed_config {
                                            Ok(yaml) => {
                                                db.admin
                                                    .send(AdminCommand::RegisterAppservice(yaml));
                                            }
                                            Err(e) => {
                                                db.admin.send(AdminCommand::SendMessage(
                                                    message::MessageEventContent::text_plain(
                                                        format!(
                                                            "Could not parse appservice config: {}",
                                                            e
                                                        ),
                                                    ),
                                                ));
                                            }
                                        }
                                    } else {
                                        db.admin.send(AdminCommand::SendMessage(
                                            message::MessageEventContent::text_plain(
                                                "Expected code block in command body.",
                                            ),
                                        ));
                                    }
                                }
                                "list_appservices" => {
                                    db.admin.send(AdminCommand::ListAppservices);
                                }
                                _ => {
                                    db.admin.send(AdminCommand::SendMessage(
                                        message::MessageEventContent::text_plain(format!(
                                            "Command: {}, Args: {:?}",
                                            command, args
                                        )),
                                    ));
                                }
                            }
                        }
                    }
                }
            }
            _ => {}
        }

        Ok(())
    }

    pub fn reset_notification_counts(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_notificationcount
            .insert(&userroom_id, &0_u64.to_be_bytes())?;
        self.userroomid_highlightcount
            .insert(&userroom_id, &0_u64.to_be_bytes())?;

        Ok(())
    }

    pub fn notification_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_notificationcount
            .get(&userroom_id)?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid notification count in db."))
            })
            .unwrap_or(Ok(0))
    }

    pub fn highlight_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_highlightcount
            .get(&userroom_id)?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid highlight count in db."))
            })
            .unwrap_or(Ok(0))
    }

    /// Generates a new StateHash and associates it with the incoming event.
    ///
    /// This adds all current state events (not including the incoming event)
    /// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
    pub fn set_event_state(
        &self,
        event_id: &EventId,
        state: &StateMap<Arc<PduEvent>>,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let shorteventid = match self.eventid_shorteventid.get(event_id.as_bytes())? {
            Some(shorteventid) => shorteventid.to_vec(),
            None => {
                let shorteventid = globals.next_count()?;
                self.eventid_shorteventid
                    .insert(event_id.as_bytes(), &shorteventid.to_be_bytes())?;
                self.shorteventid_eventid
                    .insert(&shorteventid.to_be_bytes(), event_id.as_bytes())?;
                shorteventid.to_be_bytes().to_vec()
            }
        };

        let state_hash = self.calculate_hash(
            &state
                .values()
                .map(|pdu| pdu.event_id.as_bytes())
                .collect::<Vec<_>>(),
        );

        let shortstatehash = match self.statehash_shortstatehash.get(&state_hash)? {
            Some(shortstatehash) => {
                // State already existed in db
                self.shorteventid_shortstatehash
                    .insert(shorteventid, &*shortstatehash)?;
                return Ok(());
            }
            None => {
                let shortstatehash = globals.next_count()?;
                self.statehash_shortstatehash
                    .insert(&state_hash, &shortstatehash.to_be_bytes())?;
                shortstatehash.to_be_bytes().to_vec()
            }
        };

        for ((event_type, state_key), pdu) in state {
            let mut statekey = event_type.as_ref().as_bytes().to_vec();
            statekey.push(0xff);
            statekey.extend_from_slice(&state_key.as_bytes());

            let shortstatekey = match self.statekey_shortstatekey.get(&statekey)? {
                Some(shortstatekey) => shortstatekey.to_vec(),
                None => {
                    let shortstatekey = globals.next_count()?;
                    self.statekey_shortstatekey
                        .insert(&statekey, &shortstatekey.to_be_bytes())?;
                    shortstatekey.to_be_bytes().to_vec()
                }
            };

            let shorteventid = match self.eventid_shorteventid.get(pdu.event_id.as_bytes())? {
                Some(shorteventid) => shorteventid.to_vec(),
                None => {
                    let shorteventid = globals.next_count()?;
                    self.eventid_shorteventid
                        .insert(pdu.event_id.as_bytes(), &shorteventid.to_be_bytes())?;
                    self.shorteventid_eventid
                        .insert(&shorteventid.to_be_bytes(), pdu.event_id.as_bytes())?;
                    shorteventid.to_be_bytes().to_vec()
                }
            };

            let mut state_id = shortstatehash.clone();
            state_id.extend_from_slice(&shortstatekey);

            self.stateid_shorteventid
                .insert(&*state_id, &*shorteventid)?;
        }

        self.shorteventid_shortstatehash
            .insert(shorteventid, &*shortstatehash)?;

        Ok(())
    }

    /// Generates a new StateHash and associates it with the incoming event.
    ///
    /// This adds all current state events (not including the incoming event)
    /// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
    pub fn append_to_state(
        &self,
        new_pdu: &PduEvent,
        globals: &super::globals::Globals,
    ) -> Result<u64> {
        let old_state = if let Some(old_shortstatehash) =
            self.roomid_shortstatehash.get(new_pdu.room_id.as_bytes())?
        {
            // Store state for event. The state does not include the event itself.
            // Instead it's the state before the pdu, so the room's old state.

            let shorteventid = match self.eventid_shorteventid.get(new_pdu.event_id.as_bytes())? {
                Some(shorteventid) => shorteventid.to_vec(),
                None => {
                    let shorteventid = globals.next_count()?;
                    self.eventid_shorteventid
                        .insert(new_pdu.event_id.as_bytes(), &shorteventid.to_be_bytes())?;
                    self.shorteventid_eventid
                        .insert(&shorteventid.to_be_bytes(), new_pdu.event_id.as_bytes())?;
                    shorteventid.to_be_bytes().to_vec()
                }
            };

            self.shorteventid_shortstatehash
                .insert(shorteventid, &old_shortstatehash)?;
            if new_pdu.state_key.is_none() {
                return utils::u64_from_bytes(&old_shortstatehash).map_err(|_| {
                    Error::bad_database("Invalid shortstatehash in roomid_shortstatehash.")
                });
            }

            self.stateid_shorteventid
                .scan_prefix(&old_shortstatehash)
                .filter_map(|pdu| pdu.map_err(|e| error!("{}", e)).ok())
                // Chop the old_shortstatehash out leaving behind the short state key
                .map(|(k, v)| (k[old_shortstatehash.len()..].to_vec(), v))
                .collect::<HashMap<Vec<u8>, IVec>>()
        } else {
            HashMap::new()
        };

        if let Some(state_key) = &new_pdu.state_key {
            let mut new_state: HashMap<Vec<u8>, IVec> = old_state;

            let mut new_state_key = new_pdu.kind.as_ref().as_bytes().to_vec();
            new_state_key.push(0xff);
            new_state_key.extend_from_slice(state_key.as_bytes());

            let shortstatekey = match self.statekey_shortstatekey.get(&new_state_key)? {
                Some(shortstatekey) => shortstatekey.to_vec(),
                None => {
                    let shortstatekey = globals.next_count()?;
                    self.statekey_shortstatekey
                        .insert(&new_state_key, &shortstatekey.to_be_bytes())?;
                    shortstatekey.to_be_bytes().to_vec()
                }
            };

            let shorteventid = match self.eventid_shorteventid.get(new_pdu.event_id.as_bytes())? {
                Some(shorteventid) => shorteventid.to_vec(),
                None => {
                    let shorteventid = globals.next_count()?;
                    self.eventid_shorteventid
                        .insert(new_pdu.event_id.as_bytes(), &shorteventid.to_be_bytes())?;
                    self.shorteventid_eventid
                        .insert(&shorteventid.to_be_bytes(), new_pdu.event_id.as_bytes())?;
                    shorteventid.to_be_bytes().to_vec()
                }
            };

            new_state.insert(shortstatekey, shorteventid.into());

            let new_state_hash = self.calculate_hash(
                &new_state
                    .values()
                    .map(|event_id| &**event_id)
                    .collect::<Vec<_>>(),
            );

            let shortstatehash = match self.statehash_shortstatehash.get(&new_state_hash)? {
                Some(shortstatehash) => {
                    warn!("state hash already existed?!");
                    utils::u64_from_bytes(&shortstatehash)
                        .map_err(|_| Error::bad_database("PDU has invalid count bytes."))?
                }
                None => {
                    let shortstatehash = globals.next_count()?;
                    self.statehash_shortstatehash
                        .insert(&new_state_hash, &shortstatehash.to_be_bytes())?;
                    shortstatehash
                }
            };

            for (shortstatekey, shorteventid) in new_state {
                let mut state_id = shortstatehash.to_be_bytes().to_vec();
                state_id.extend_from_slice(&shortstatekey);
                self.stateid_shorteventid.insert(&state_id, &shorteventid)?;
            }

            Ok(shortstatehash)
        } else {
            Err(Error::bad_database(
                "Tried to insert non-state event into room without a state.",
            ))
        }
    }

    pub fn calculate_invite_state(
        &self,
        invite_event: &PduEvent,
    ) -> Result<Vec<Raw<AnyStrippedStateEvent>>> {
        let mut state = Vec::new();
        // Add recommended events
        if let Some(e) = self.room_state_get(&invite_event.room_id, &EventType::RoomCreate, "")? {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) =
            self.room_state_get(&invite_event.room_id, &EventType::RoomJoinRules, "")?
        {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) =
            self.room_state_get(&invite_event.room_id, &EventType::RoomCanonicalAlias, "")?
        {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) = self.room_state_get(&invite_event.room_id, &EventType::RoomAvatar, "")? {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) = self.room_state_get(&invite_event.room_id, &EventType::RoomName, "")? {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) = self.room_state_get(
            &invite_event.room_id,
            &EventType::RoomMember,
            invite_event.sender.as_str(),
        )? {
            state.push(e.to_stripped_state_event());
        }

        state.push(invite_event.to_stripped_state_event());
        Ok(state)
    }

    pub fn set_room_state(&self, room_id: &RoomId, shortstatehash: u64) -> Result<()> {
        self.roomid_shortstatehash
            .insert(room_id.as_bytes(), &shortstatehash.to_be_bytes())?;

        Ok(())
    }

    /// Creates a new persisted data unit and adds it to a room.
    pub fn build_and_append_pdu(
        &self,
        pdu_builder: PduBuilder,
        sender: &UserId,
        room_id: &RoomId,
        db: &Database,
    ) -> Result<EventId> {
        let PduBuilder {
            event_type,
            content,
            unsigned,
            state_key,
            redacts,
        } = pdu_builder;
        // TODO: Make sure this isn't called twice in parallel
        let prev_events = self
            .get_pdu_leaves(&room_id)?
            .into_iter()
            .take(20)
            .collect::<Vec<_>>();

        let create_event = self.room_state_get(&room_id, &EventType::RoomCreate, "")?;

        let create_event_content = create_event
            .as_ref()
            .map(|create_event| {
                Ok::<_, Error>(
                    serde_json::from_value::<Raw<CreateEventContent>>(create_event.content.clone())
                        .expect("Raw::from_value always works.")
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid PowerLevels event in db."))?,
                )
            })
            .transpose()?;

        let create_prev_event = if prev_events.len() == 1
            && Some(&prev_events[0]) == create_event.as_ref().map(|c| &c.event_id)
        {
            create_event.map(Arc::new)
        } else {
            None
        };

        // If there was no create event yet, assume we are creating a version 6 room right now
        let room_version_id = create_event_content
            .map_or(RoomVersionId::Version6, |create_event| {
                create_event.room_version
            });
        let room_version = RoomVersion::new(&room_version_id).expect("room version is supported");

        let auth_events = self.get_auth_events(
            &room_id,
            &event_type,
            &sender,
            state_key.as_deref(),
            &content,
        )?;

        // Our depth is the maximum depth of prev_events + 1
        let depth = prev_events
            .iter()
            .filter_map(|event_id| Some(self.get_pdu(event_id).ok()??.depth))
            .max()
            .unwrap_or_else(|| uint!(0))
            + uint!(1);

        let mut unsigned = unsigned.unwrap_or_default();
        if let Some(state_key) = &state_key {
            if let Some(prev_pdu) = self.room_state_get(&room_id, &event_type, &state_key)? {
                unsigned.insert("prev_content".to_owned(), prev_pdu.content);
                unsigned.insert(
                    "prev_sender".to_owned(),
                    serde_json::to_value(prev_pdu.sender).expect("UserId::to_value always works"),
                );
            }
        }

        let mut pdu = PduEvent {
            event_id: ruma::event_id!("$thiswillbefilledinlater"),
            room_id: room_id.clone(),
            sender: sender.clone(),
            origin_server_ts: utils::millis_since_unix_epoch()
                .try_into()
                .expect("time is valid"),
            kind: event_type,
            content,
            state_key,
            prev_events,
            depth,
            auth_events: auth_events
                .iter()
                .map(|(_, pdu)| pdu.event_id.clone())
                .collect(),
            redacts,
            unsigned,
            hashes: ruma::events::pdu::EventHash {
                sha256: "aaa".to_owned(),
            },
            signatures: BTreeMap::new(),
        };

        let auth_check = state_res::auth_check(
            &room_version,
            &Arc::new(pdu.clone()),
            create_prev_event,
            &auth_events,
            None, // TODO: third_party_invite
        )
        .map_err(|e| {
            error!("{:?}", e);
            Error::bad_database("Auth check failed.")
        })?;

        if !auth_check {
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "Event is not authorized.",
            ));
        }

        // Hash and sign
        let mut pdu_json =
            utils::to_canonical_object(&pdu).expect("event is valid, we just created it");

        pdu_json.remove("event_id");

        // Add origin because synapse likes that (and it's required in the spec)
        pdu_json.insert(
            "origin".to_owned(),
            CanonicalJsonValue::String(db.globals.server_name().as_ref().to_owned()),
        );

        ruma::signatures::hash_and_sign_event(
            db.globals.server_name().as_str(),
            db.globals.keypair(),
            &mut pdu_json,
            &room_version_id,
        )
        .expect("event is valid, we just created it");

        // Generate event id
        pdu.event_id = EventId::try_from(&*format!(
            "${}",
            ruma::signatures::reference_hash(&pdu_json, &room_version_id)
                .expect("ruma can calculate reference hashes")
        ))
        .expect("ruma's reference hashes are valid event ids");

        pdu_json.insert(
            "event_id".to_owned(),
            CanonicalJsonValue::String(pdu.event_id.as_str().to_owned()),
        );

        // Increment the last index and use that
        // This is also the next_batch/since value
        let count = db.globals.next_count()?;
        let mut pdu_id = room_id.as_bytes().to_vec();
        pdu_id.push(0xff);
        pdu_id.extend_from_slice(&count.to_be_bytes());

        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        let statehashid = self.append_to_state(&pdu, &db.globals)?;

        self.append_pdu(
            &pdu,
            pdu_json,
            count,
            pdu_id.clone().into(),
            // Since this PDU references all pdu_leaves we can update the leaves
            // of the room
            &[pdu.event_id.clone()],
            db,
        )?;

        // We set the room state after inserting the pdu, so that we never have a moment in time
        // where events in the current room state do not exist
        self.set_room_state(&room_id, statehashid)?;

        for server in self
            .room_servers(room_id)
            .filter_map(|r| r.ok())
            .filter(|server| &**server != db.globals.server_name())
        {
            db.sending.send_pdu(&server, &pdu_id)?;
        }

        for appservice in db.appservice.iter_all().filter_map(|r| r.ok()) {
            if let Some(namespaces) = appservice.1.get("namespaces") {
                let users = namespaces
                    .get("users")
                    .and_then(|users| users.as_sequence())
                    .map_or_else(Vec::new, |users| {
                        users
                            .iter()
                            .map(|users| {
                                users
                                    .get("regex")
                                    .and_then(|regex| regex.as_str())
                                    .and_then(|regex| Regex::new(regex).ok())
                            })
                            .filter_map(|o| o)
                            .collect::<Vec<_>>()
                    });
                let aliases = namespaces
                    .get("aliases")
                    .and_then(|aliases| aliases.as_sequence())
                    .map_or_else(Vec::new, |aliases| {
                        aliases
                            .iter()
                            .map(|aliases| {
                                aliases
                                    .get("regex")
                                    .and_then(|regex| regex.as_str())
                                    .and_then(|regex| Regex::new(regex).ok())
                            })
                            .filter_map(|o| o)
                            .collect::<Vec<_>>()
                    });
                let rooms = namespaces
                    .get("rooms")
                    .and_then(|rooms| rooms.as_sequence());

                let bridge_user_id = appservice
                    .1
                    .get("sender_localpart")
                    .and_then(|string| string.as_str())
                    .and_then(|string| {
                        UserId::parse_with_server_name(string, db.globals.server_name()).ok()
                    });

                let user_is_joined =
                    |bridge_user_id| self.is_joined(&bridge_user_id, room_id).unwrap_or(false);

                let matching_users = |users: &Regex| {
                    users.is_match(pdu.sender.as_str())
                        || pdu.kind == EventType::RoomMember
                            && pdu
                                .state_key
                                .as_ref()
                                .map_or(false, |state_key| users.is_match(&state_key))
                        || self.room_members(&room_id).any(|userid| {
                            userid.map_or(false, |userid| users.is_match(userid.as_str()))
                        })
                };
                let matching_aliases = |aliases: &Regex| {
                    self.room_aliases(&room_id)
                        .filter_map(|r| r.ok())
                        .any(|room_alias| aliases.is_match(room_alias.as_str()))
                };

                if bridge_user_id.map_or(false, user_is_joined)
                    || aliases.iter().any(matching_aliases)
                    || rooms.map_or(false, |rooms| rooms.contains(&room_id.as_str().into()))
                    || users.iter().any(matching_users)
                {
                    db.sending.send_pdu_appservice(&appservice.0, &pdu_id)?;
                }
            }
        }

        Ok(pdu.event_id)
    }

    /// Returns an iterator over all PDUs in a room.
    #[tracing::instrument(skip(self))]
    pub fn all_pdus(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<impl Iterator<Item = Result<(IVec, PduEvent)>>> {
        self.pdus_since(user_id, room_id, 0)
    }

    /// Returns a double-ended iterator over all events in a room that happened after the event with id `since`
    /// in chronological order.
    #[tracing::instrument(skip(self))]
    pub fn pdus_since(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<impl DoubleEndedIterator<Item = Result<(IVec, PduEvent)>>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        // Skip the first pdu if it's exactly at since, because we sent that last time
        let mut first_pdu_id = prefix.clone();
        first_pdu_id.extend_from_slice(&(since + 1).to_be_bytes());

        let mut last_pdu_id = prefix;
        last_pdu_id.extend_from_slice(&u64::MAX.to_be_bytes());

        let user_id = user_id.clone();
        Ok(self
            .pduid_pdu
            .range(first_pdu_id..last_pdu_id)
            .filter_map(|r| r.ok())
            .map(move |(pdu_id, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((pdu_id, pdu))
            }))
    }

    /// Returns an iterator over all events and their tokens in a room that happened before the
    /// event with id `until` in reverse-chronological order.
    pub fn pdus_until(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        until: u64,
    ) -> impl Iterator<Item = Result<(IVec, PduEvent)>> {
        // Create the first part of the full pdu id
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut current = prefix.clone();
        current.extend_from_slice(&until.to_be_bytes());

        let current: &[u8] = &current;

        let user_id = user_id.clone();
        self.pduid_pdu
            .range(..current)
            .rev()
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(pdu_id, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((pdu_id, pdu))
            })
    }

    /// Returns an iterator over all events and their token in a room that happened after the event
    /// with id `from` in chronological order.
    #[tracing::instrument(skip(self))]
    pub fn pdus_after(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        from: u64,
    ) -> impl Iterator<Item = Result<(IVec, PduEvent)>> {
        // Create the first part of the full pdu id
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut current = prefix.clone();
        current.extend_from_slice(&(from + 1).to_be_bytes()); // +1 so we don't send the base event

        let current: &[u8] = &current;

        let user_id = user_id.clone();
        self.pduid_pdu
            .range(current..)
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(pdu_id, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((pdu_id, pdu))
            })
    }

    /// Replace a PDU with the redacted form.
    pub fn redact_pdu(&self, event_id: &EventId, reason: &PduEvent) -> Result<()> {
        if let Some(pdu_id) = self.get_pdu_id(event_id)? {
            let mut pdu = self
                .get_pdu_from_id(&pdu_id)?
                .ok_or_else(|| Error::bad_database("PDU ID points to invalid PDU."))?;
            pdu.redact(&reason)?;
            self.replace_pdu(&pdu_id, &pdu)?;
            Ok(())
        } else {
            Err(Error::BadRequest(
                ErrorKind::NotFound,
                "Event ID does not exist.",
            ))
        }
    }

    /// Update current membership data.
    pub fn update_membership(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        membership: member::MembershipState,
        sender: &UserId,
        last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
        db: &Database,
    ) -> Result<()> {
        let mut roomserver_id = room_id.as_bytes().to_vec();
        roomserver_id.push(0xff);
        roomserver_id.extend_from_slice(user_id.server_name().as_bytes());

        let mut serverroom_id = user_id.server_name().as_bytes().to_vec();
        serverroom_id.push(0xff);
        serverroom_id.extend_from_slice(room_id.as_bytes());

        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        match &membership {
            member::MembershipState::Join => {
                // Check if the user never joined this room
                if !self.once_joined(&user_id, &room_id)? {
                    // Add the user ID to the join list then
                    self.roomuseroncejoinedids.insert(&userroom_id, &[])?;

                    // Check if the room has a predecessor
                    if let Some(predecessor) = self
                        .room_state_get(&room_id, &EventType::RoomCreate, "")?
                        .and_then(|create| {
                            serde_json::from_value::<
                                Raw<ruma::events::room::create::CreateEventContent>,
                            >(create.content)
                            .expect("Raw::from_value always works")
                            .deserialize()
                            .ok()
                        })
                        .and_then(|content| content.predecessor)
                    {
                        // Copy user settings from predecessor to the current room:
                        // - Push rules
                        //
                        // TODO: finish this once push rules are implemented.
                        //
                        // let mut push_rules_event_content = account_data
                        //     .get::<ruma::events::push_rules::PushRulesEvent>(
                        //         None,
                        //         user_id,
                        //         EventType::PushRules,
                        //     )?;
                        //
                        // NOTE: find where `predecessor.room_id` match
                        //       and update to `room_id`.
                        //
                        // account_data
                        //     .update(
                        //         None,
                        //         user_id,
                        //         EventType::PushRules,
                        //         &push_rules_event_content,
                        //         globals,
                        //     )
                        //     .ok();

                        // Copy old tags to new room
                        if let Some(tag_event) =
                            db.account_data.get::<ruma::events::tag::TagEvent>(
                                Some(&predecessor.room_id),
                                user_id,
                                EventType::Tag,
                            )?
                        {
                            db.account_data
                                .update(
                                    Some(room_id),
                                    user_id,
                                    EventType::Tag,
                                    &tag_event,
                                    &db.globals,
                                )
                                .ok();
                        };

                        // Copy direct chat flag
                        if let Some(mut direct_event) =
                            db.account_data.get::<ruma::events::direct::DirectEvent>(
                                None,
                                user_id,
                                EventType::Direct,
                            )?
                        {
                            let mut room_ids_updated = false;

                            for room_ids in direct_event.content.0.values_mut() {
                                if room_ids.iter().any(|r| r == &predecessor.room_id) {
                                    room_ids.push(room_id.clone());
                                    room_ids_updated = true;
                                }
                            }

                            if room_ids_updated {
                                db.account_data.update(
                                    None,
                                    user_id,
                                    EventType::Direct,
                                    &direct_event,
                                    &db.globals,
                                )?;
                            }
                        };
                    }
                }

                self.roomserverids.insert(&roomserver_id, &[])?;
                self.serverroomids.insert(&serverroom_id, &[])?;
                self.userroomid_joined.insert(&userroom_id, &[])?;
                self.roomuserid_joined.insert(&roomuser_id, &[])?;
                self.userroomid_invitestate.remove(&userroom_id)?;
                self.roomuserid_invitecount.remove(&roomuser_id)?;
                self.userroomid_leftstate.remove(&userroom_id)?;
                self.roomuserid_leftcount.remove(&roomuser_id)?;
            }
            member::MembershipState::Invite => {
                // We want to know if the sender is ignored by the receiver
                let is_ignored = db
                    .account_data
                    .get::<ignored_user_list::IgnoredUserListEvent>(
                        None,     // Ignored users are in global account data
                        &user_id, // Receiver
                        EventType::IgnoredUserList,
                    )?
                    .map_or(false, |ignored| {
                        ignored.content.ignored_users.contains(&sender)
                    });

                if is_ignored {
                    return Ok(());
                }

                self.roomserverids.insert(&roomserver_id, &[])?;
                self.serverroomids.insert(&serverroom_id, &[])?;
                self.userroomid_invitestate.insert(
                    &userroom_id,
                    serde_json::to_vec(&last_state.unwrap_or_default())
                        .expect("state to bytes always works"),
                )?;
                self.roomuserid_invitecount
                    .insert(&roomuser_id, &db.globals.next_count()?.to_be_bytes())?;
                self.userroomid_joined.remove(&userroom_id)?;
                self.roomuserid_joined.remove(&roomuser_id)?;
                self.userroomid_leftstate.remove(&userroom_id)?;
                self.roomuserid_leftcount.remove(&roomuser_id)?;
            }
            member::MembershipState::Leave | member::MembershipState::Ban => {
                if self
                    .room_members(room_id)
                    .chain(self.room_members_invited(room_id))
                    .filter_map(|r| r.ok())
                    .all(|u| u.server_name() != user_id.server_name())
                {
                    self.roomserverids.remove(&roomserver_id)?;
                    self.serverroomids.remove(&serverroom_id)?;
                }
                self.userroomid_leftstate.insert(
                    &userroom_id,
                    serde_json::to_vec(&Vec::<Raw<AnySyncStateEvent>>::new()).unwrap(),
                )?; // TODO
                self.roomuserid_leftcount
                    .insert(&roomuser_id, &db.globals.next_count()?.to_be_bytes())?;
                self.userroomid_joined.remove(&userroom_id)?;
                self.roomuserid_joined.remove(&roomuser_id)?;
                self.userroomid_invitestate.remove(&userroom_id)?;
                self.roomuserid_invitecount.remove(&roomuser_id)?;
            }
            _ => {}
        }

        Ok(())
    }

    pub async fn leave_room(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        db: &Database,
    ) -> Result<()> {
        // Ask a remote server if we don't have this room
        if !self.exists(room_id)? && room_id.server_name() != db.globals.server_name() {
            if let Err(e) = self.remote_leave_room(user_id, room_id, db).await {
                warn!("Failed to leave room {} remotely: {}", user_id, e);
                // Don't tell the client about this error
            }

            let last_state = self
                .invite_state(user_id, room_id)?
                .map_or_else(|| self.left_state(user_id, room_id), |s| Ok(Some(s)))?;

            // We always drop the invite, we can't rely on other servers
            self.update_membership(
                room_id,
                user_id,
                MembershipState::Leave,
                user_id,
                last_state,
                db,
            )?;
        } else {
            let mut event = serde_json::from_value::<Raw<member::MemberEventContent>>(
                self.room_state_get(room_id, &EventType::RoomMember, &user_id.to_string())?
                    .ok_or(Error::BadRequest(
                        ErrorKind::BadState,
                        "Cannot leave a room you are not a member of.",
                    ))?
                    .content,
            )
            .expect("from_value::<Raw<..>> can never fail")
            .deserialize()
            .map_err(|_| Error::bad_database("Invalid member event in database."))?;

            event.membership = member::MembershipState::Leave;

            self.build_and_append_pdu(
                PduBuilder {
                    event_type: EventType::RoomMember,
                    content: serde_json::to_value(event)
                        .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some(user_id.to_string()),
                    redacts: None,
                },
                user_id,
                room_id,
                db,
            )?;
        }

        Ok(())
    }

    async fn remote_leave_room(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        db: &Database,
    ) -> Result<()> {
        let mut make_leave_response_and_server = Err(Error::BadServerResponse(
            "No server available to assist in leaving.",
        ));

        let invite_state = db
            .rooms
            .invite_state(user_id, room_id)?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "User is not invited.",
            ))?;

        let servers = invite_state
            .iter()
            .filter_map(|event| {
                serde_json::from_str::<serde_json::Value>(&event.json().to_string()).ok()
            })
            .filter_map(|event| event.get("sender").cloned())
            .filter_map(|sender| sender.as_str().map(|s| s.to_owned()))
            .filter_map(|sender| UserId::try_from(sender).ok())
            .map(|user| user.server_name().to_owned())
            .collect::<HashSet<_>>();

        for remote_server in servers {
            let make_leave_response = db
                .sending
                .send_federation_request(
                    &db.globals,
                    &remote_server,
                    federation::membership::get_leave_event::v1::Request { room_id, user_id },
                )
                .await;

            make_leave_response_and_server = make_leave_response.map(|r| (r, remote_server));

            if make_leave_response_and_server.is_ok() {
                break;
            }
        }

        let (make_leave_response, remote_server) = make_leave_response_and_server?;

        let room_version_id = match make_leave_response.room_version {
            Some(id @ RoomVersionId::Version6) => id,
            _ => return Err(Error::BadServerResponse("Room version is not supported")),
        };

        let mut leave_event_stub =
            serde_json::from_str::<CanonicalJsonObject>(make_leave_response.event.json().get())
                .map_err(|_| {
                    Error::BadServerResponse("Invalid make_leave event json received from server.")
                })?;

        // TODO: Is origin needed?
        leave_event_stub.insert(
            "origin".to_owned(),
            CanonicalJsonValue::String(db.globals.server_name().as_str().to_owned()),
        );
        leave_event_stub.insert(
            "origin_server_ts".to_owned(),
            CanonicalJsonValue::Integer(
                utils::millis_since_unix_epoch()
                    .try_into()
                    .expect("Timestamp is valid js_int value"),
            ),
        );
        // We don't leave the event id in the pdu because that's only allowed in v1 or v2 rooms
        leave_event_stub.remove("event_id");

        // In order to create a compatible ref hash (EventID) the `hashes` field needs to be present
        ruma::signatures::hash_and_sign_event(
            db.globals.server_name().as_str(),
            db.globals.keypair(),
            &mut leave_event_stub,
            &room_version_id,
        )
        .expect("event is valid, we just created it");

        // Generate event id
        let event_id = EventId::try_from(&*format!(
            "${}",
            ruma::signatures::reference_hash(&leave_event_stub, &room_version_id)
                .expect("ruma can calculate reference hashes")
        ))
        .expect("ruma's reference hashes are valid event ids");

        // Add event_id back
        leave_event_stub.insert(
            "event_id".to_owned(),
            CanonicalJsonValue::String(event_id.as_str().to_owned()),
        );

        // It has enough fields to be called a proper event now
        let leave_event = leave_event_stub;

        db.sending
            .send_federation_request(
                &db.globals,
                &remote_server,
                federation::membership::create_leave_event::v2::Request {
                    room_id,
                    event_id: &event_id,
                    pdu: PduEvent::convert_to_outgoing_federation_event(leave_event.clone()),
                },
            )
            .await?;

        Ok(())
    }

    /// Makes a user forget a room.
    pub fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        self.userroomid_leftstate.remove(userroom_id)?;
        self.roomuserid_leftcount.remove(roomuser_id)?;

        Ok(())
    }

    pub fn set_alias(
        &self,
        alias: &RoomAliasId,
        room_id: Option<&RoomId>,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        if let Some(room_id) = room_id {
            // New alias
            self.alias_roomid
                .insert(alias.alias(), room_id.as_bytes())?;
            let mut aliasid = room_id.as_bytes().to_vec();
            aliasid.push(0xff);
            aliasid.extend_from_slice(&globals.next_count()?.to_be_bytes());
            self.aliasid_alias.insert(aliasid, &*alias.as_bytes())?;
        } else {
            // room_id=None means remove alias
            let room_id = self
                .alias_roomid
                .remove(alias.alias())?
                .ok_or(Error::BadRequest(
                    ErrorKind::NotFound,
                    "Alias does not exist.",
                ))?;

            let mut prefix = room_id.to_vec();
            prefix.push(0xff);

            for key in self.aliasid_alias.scan_prefix(prefix).keys() {
                self.aliasid_alias.remove(key?)?;
            }
        }

        Ok(())
    }

    pub fn id_from_alias(&self, alias: &RoomAliasId) -> Result<Option<RoomId>> {
        self.alias_roomid
            .get(alias.alias())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(
                    RoomId::try_from(utils::string_from_bytes(&bytes).map_err(|_| {
                        Error::bad_database("Room ID in alias_roomid is invalid unicode.")
                    })?)
                    .map_err(|_| Error::bad_database("Room ID in alias_roomid is invalid."))?,
                ))
            })
    }

    pub fn room_aliases(&self, room_id: &RoomId) -> impl Iterator<Item = Result<RoomAliasId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.aliasid_alias
            .scan_prefix(prefix)
            .values()
            .map(|bytes| {
                Ok(utils::string_from_bytes(&bytes?)
                    .map_err(|_| Error::bad_database("Invalid alias bytes in aliasid_alias."))?
                    .try_into()
                    .map_err(|_| Error::bad_database("Invalid alias in aliasid_alias."))?)
            })
    }

    pub fn set_public(&self, room_id: &RoomId, public: bool) -> Result<()> {
        if public {
            self.publicroomids.insert(room_id.as_bytes(), &[])?;
        } else {
            self.publicroomids.remove(room_id.as_bytes())?;
        }

        Ok(())
    }

    pub fn is_public_room(&self, room_id: &RoomId) -> Result<bool> {
        Ok(self.publicroomids.contains_key(room_id.as_bytes())?)
    }

    pub fn public_rooms(&self) -> impl Iterator<Item = Result<RoomId>> {
        self.publicroomids.iter().keys().map(|bytes| {
            Ok(
                RoomId::try_from(utils::string_from_bytes(&bytes?).map_err(|_| {
                    Error::bad_database("Room ID in publicroomids is invalid unicode.")
                })?)
                .map_err(|_| Error::bad_database("Room ID in publicroomids is invalid."))?,
            )
        })
    }

    pub fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<(impl Iterator<Item = Vec<u8>> + 'a, Vec<String>)> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let words = search_string
            .split_terminator(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .collect::<Vec<_>>();

        let iterators = words.clone().into_iter().map(move |word| {
            let mut prefix2 = prefix.clone();
            prefix2.extend_from_slice(word.as_bytes());
            prefix2.push(0xff);
            self.tokenids
                .scan_prefix(&prefix2)
                .keys()
                .rev() // Newest pdus first
                .filter_map(|r| r.ok())
                .map(|key| {
                    let pduid_index = key
                        .iter()
                        .enumerate()
                        .filter(|(_, &b)| b == 0xff)
                        .nth(1)
                        .ok_or_else(|| Error::bad_database("Invalid tokenid in db."))?
                        .0
                        + 1; // +1 because the pdu id starts AFTER the separator

                    let pdu_id = key[pduid_index..].to_vec();

                    Ok::<_, Error>(pdu_id)
                })
                .filter_map(|r| r.ok())
        });

        Ok((
            utils::common_elements(iterators, |a, b| {
                // We compare b with a because we reversed the iterator earlier
                b.cmp(a)
            })
            .unwrap(),
            words,
        ))
    }

    #[tracing::instrument(skip(self))]
    pub fn get_shared_rooms<'a>(
        &'a self,
        users: Vec<UserId>,
    ) -> impl Iterator<Item = Result<RoomId>> + 'a {
        let iterators = users.into_iter().map(move |user_id| {
            let mut prefix = user_id.as_bytes().to_vec();
            prefix.push(0xff);

            self.userroomid_joined
                .scan_prefix(&prefix)
                .keys()
                .filter_map(|r| r.ok())
                .map(|key| {
                    let roomid_index = key
                        .iter()
                        .enumerate()
                        .find(|(_, &b)| b == 0xff)
                        .ok_or_else(|| Error::bad_database("Invalid userroomid_joined in db."))?
                        .0
                        + 1; // +1 because the room id starts AFTER the separator

                    let room_id = key[roomid_index..].to_vec();

                    Ok::<_, Error>(room_id)
                })
                .filter_map(|r| r.ok())
        });

        // We use the default compare function because keys are sorted correctly (not reversed)
        utils::common_elements(iterators, Ord::cmp)
            .expect("users is not empty")
            .map(|bytes| {
                RoomId::try_from(utils::string_from_bytes(&*bytes).map_err(|_| {
                    Error::bad_database("Invalid RoomId bytes in userroomid_joined")
                })?)
                .map_err(|_| Error::bad_database("Invalid RoomId in userroomid_joined."))
            })
    }

    /// Returns an iterator of all servers participating in this room.
    pub fn room_servers(&self, room_id: &RoomId) -> impl Iterator<Item = Result<Box<ServerName>>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomserverids.scan_prefix(prefix).keys().map(|key| {
            Ok(Box::<ServerName>::try_from(
                utils::string_from_bytes(
                    &key?
                        .rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| {
                    Error::bad_database("Server name in roomserverids is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Server name in roomserverids is invalid."))?)
        })
    }

    /// Returns an iterator of all rooms a server participates in (as far as we know).
    pub fn server_rooms(&self, server: &ServerName) -> impl Iterator<Item = Result<RoomId>> {
        let mut prefix = server.as_bytes().to_vec();
        prefix.push(0xff);

        self.serverroomids.scan_prefix(prefix).keys().map(|key| {
            Ok(RoomId::try_from(
                utils::string_from_bytes(
                    &key?
                        .rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| Error::bad_database("RoomId in serverroomids is invalid unicode."))?,
            )
            .map_err(|_| Error::bad_database("RoomId in serverroomids is invalid."))?)
        })
    }

    /// Returns an iterator over all joined members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuserid_joined
            .scan_prefix(prefix)
            .keys()
            .map(|key| {
                Ok(UserId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in roomuserid_joined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in roomuserid_joined is invalid."))?)
            })
    }

    /// Returns an iterator over all User IDs who ever joined a room.
    pub fn room_useroncejoined(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuseroncejoinedids
            .scan_prefix(prefix)
            .keys()
            .map(|key| {
                Ok(UserId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in room_useroncejoined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in room_useroncejoined is invalid."))?)
            })
    }

    /// Returns an iterator over all invited members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members_invited(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuserid_invitecount
            .scan_prefix(prefix)
            .keys()
            .map(|key| {
                Ok(UserId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in roomuserid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in roomuserid_invited is invalid."))?)
            })
    }

    #[tracing::instrument(skip(self))]
    pub fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_invitecount
            .get(key)?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid invitecount in db.")
                })?))
            })
    }

    #[tracing::instrument(skip(self))]
    pub fn get_left_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_leftcount
            .get(key)?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid leftcount in db.")
                })?))
            })
    }

    /// Returns an iterator over all rooms this user joined.
    #[tracing::instrument(skip(self))]
    pub fn rooms_joined(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        self.userroomid_joined
            .scan_prefix(user_id.as_bytes())
            .keys()
            .map(|key| {
                Ok(RoomId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_joined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_joined is invalid."))?)
            })
    }

    /// Returns an iterator over all rooms a user was invited to.
    #[tracing::instrument(skip(self))]
    pub fn rooms_invited(
        &self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(RoomId, Vec<Raw<AnyStrippedStateEvent>>)>> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.userroomid_invitestate.scan_prefix(prefix).map(|r| {
            let (key, state) = r?;
            let room_id = RoomId::try_from(
                utils::string_from_bytes(
                    &key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| {
                    Error::bad_database("Room ID in userroomid_invited is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Room ID in userroomid_invited is invalid."))?;

            let state = serde_json::from_slice(&state)
                .map_err(|_| Error::bad_database("Invalid state in userroomid_invitestate."))?;

            Ok((room_id, state))
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn invite_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&room_id.as_bytes());

        self.userroomid_invitestate
            .get(key)?
            .map(|state| {
                let state = serde_json::from_slice(&state)
                    .map_err(|_| Error::bad_database("Invalid state in userroomid_invitestate."))?;

                Ok(state)
            })
            .transpose()
    }

    #[tracing::instrument(skip(self))]
    pub fn left_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&room_id.as_bytes());

        self.userroomid_leftstate
            .get(key)?
            .map(|state| {
                let state = serde_json::from_slice(&state)
                    .map_err(|_| Error::bad_database("Invalid state in userroomid_leftstate."))?;

                Ok(state)
            })
            .transpose()
    }

    /// Returns an iterator over all rooms a user left.
    #[tracing::instrument(skip(self))]
    pub fn rooms_left(
        &self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(RoomId, Vec<Raw<AnySyncStateEvent>>)>> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.userroomid_leftstate.scan_prefix(prefix).map(|r| {
            let (key, state) = r?;
            let room_id = RoomId::try_from(
                utils::string_from_bytes(
                    &key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| {
                    Error::bad_database("Room ID in userroomid_invited is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Room ID in userroomid_invited is invalid."))?;

            let state = serde_json::from_slice(&state)
                .map_err(|_| Error::bad_database("Invalid state in userroomid_leftstate."))?;

            Ok((room_id, state))
        })
    }

    pub fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.roomuseroncejoinedids.get(userroom_id)?.is_some())
    }

    pub fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_joined.get(userroom_id)?.is_some())
    }

    pub fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_invitestate.get(userroom_id)?.is_some())
    }

    pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_leftstate.get(userroom_id)?.is_some())
    }
}
