mod edus;

pub use edus::RoomEdus;

use crate::{pdu::PduBuilder, utils, Error, PduEvent, Result};
use log::error;
// TODO if ruma-signatures re-exports `use ruma::signatures::digest;`
use ring::digest;
use ruma::{
    api::client::error::ErrorKind,
    events::{
        ignored_user_list,
        room::{
            member,
            power_levels::{self, PowerLevelsEventContent},
            redaction,
        },
        EventType,
    },
    EventId, Raw, RoomAliasId, RoomId, UserId,
};
use sled::IVec;
use state_res::{event_auth, Requester, StateEvent, StateMap, StateStore};

use std::{
    collections::{BTreeMap, HashMap},
    convert::{TryFrom, TryInto},
    mem,
    result::Result as StdResult,
};

/// The unique identifier of each state group.
///
/// This is created when a state group is added to the database by
/// hashing the entire state.
pub type StateHashId = Vec<u8>;

/// This identifier consists of roomId + count. It represents a
/// unique event, it will never be overwritten or removed.
pub type PduId = IVec;

pub struct Rooms {
    pub edus: edus::RoomEdus,
    pub(super) pduid_pdu: sled::Tree, // PduId = RoomId + Count
    pub(super) eventid_pduid: sled::Tree,
    pub(super) roomid_pduleaves: sled::Tree,
    pub(super) alias_roomid: sled::Tree,
    pub(super) aliasid_alias: sled::Tree, // AliasId = RoomId + Count
    pub(super) publicroomids: sled::Tree,

    pub(super) tokenids: sled::Tree, // TokenId = RoomId + Token + PduId

    pub(super) userroomid_joined: sled::Tree,
    pub(super) roomuserid_joined: sled::Tree,
    pub(super) userroomid_invited: sled::Tree,
    pub(super) roomuserid_invited: sled::Tree,
    pub(super) userroomid_left: sled::Tree,

    // STATE TREES
    /// This holds the full current state, including the latest event.
    pub(super) roomstateid_pduid: sled::Tree, // RoomStateId = Room + StateType + StateKey
    /// This holds the full room state minus the latest event.
    pub(super) pduid_statehash: sled::Tree, // PDU id -> StateHash
    /// Also holds the full room state minus the latest event.
    pub(super) stateid_pduid: sled::Tree, // StateId = StateHash + (EventType, StateKey)
    /// The room_id -> the latest StateHash
    pub(super) roomid_statehash: sled::Tree,
}

impl StateStore for Rooms {
    fn get_event(&self, room_id: &RoomId, event_id: &EventId) -> StdResult<StateEvent, String> {
        let pid = self
            .eventid_pduid
            .get(event_id.as_bytes())
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "PDU via room_id and event_id not found in the db.".to_owned())?;

        utils::deserialize(
            &self
                .pduid_pdu
                .get(pid)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "PDU via pduid not found in db.".to_owned())?,
        )
        .and_then(|pdu: StateEvent| {
            // conduit's PDU's always contain a room_id but some
            // of ruma's do not so this must be an Option
            if pdu.room_id() == Some(room_id) {
                Ok(pdu)
            } else {
                Err(Error::bad_database("Found PDU for incorrect room in db."))
            }
        })
        .map_err(|e| e.to_string())
    }
}

impl Rooms {
    /// Builds a `StateMap` by iterating over all keys that start
    /// with `state_hash`, this gives the full state at event "x".
    pub fn get_statemap_by_hash(&self, state_hash: StateHashId) -> Result<StateMap<EventId>> {
        self.stateid_pduid
            .scan_prefix(&state_hash)
            .values()
            .map(|pduid| {
                self.pduid_pdu.get(&pduid?)?.map_or_else(
                    || Err(Error::bad_database("Failed to find StateMap.")),
                    |b| {
                        serde_json::from_slice::<PduEvent>(&b)
                            .map_err(|_| Error::bad_database("Invalid PDU in db."))
                    },
                )
            })
            .map(|pdu| {
                let pdu = pdu?;
                Ok(((pdu.kind, pdu.state_key), pdu.event_id))
            })
            .collect::<Result<StateMap<_>>>()
    }

    // TODO make this return Result
    /// Fetches the previous StateHash ID to `current`.
    pub fn prev_state_hash(&self, current: StateHashId) -> Option<StateHashId> {
        let mut found = false;
        for pair in self.pduid_statehash.iter().rev() {
            let prev = pair.ok()?.1;
            if current == prev.as_ref() {
                found = true;
            }
            if current != prev.as_ref() && found {
                return Some(prev.to_vec());
            }
        }
        None
    }

    /// Fetch the current State using the `roomstateid_pduid` tree.
    pub fn current_state_pduids(&self, room_id: &RoomId) -> Result<StateMap<PduId>> {
        // TODO this could also scan roomstateid_pduid if we passed in room_id ?
        self.roomstateid_pduid
            .scan_prefix(room_id.as_bytes())
            .values()
            .map(|pduid| {
                let pduid = &pduid?;
                self.pduid_pdu.get(pduid)?.map_or_else(
                    || {
                        Err(Error::bad_database(
                            "Failed to find current state of pduid's.",
                        ))
                    },
                    |b| {
                        Ok((
                            serde_json::from_slice::<PduEvent>(&b)
                                .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                            pduid.clone(),
                        ))
                    },
                )
            })
            .map(|pair| {
                let (pdu, id) = pair?;
                Ok(((pdu.kind, pdu.state_key), id))
            })
            .collect::<Result<StateMap<_>>>()
    }

    /// Returns the last state hash key added to the db.
    pub fn current_state_hash(&self, room_id: &RoomId) -> Result<StateHashId> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        // We must check here because this method is called outside and before
        // `append_state_pdu` so the DB can be empty
        if self.pduid_statehash.scan_prefix(prefix).next().is_none() {
            // return the hash of the room_id, this represents a room with no state
            return self.new_state_hash_id(room_id);
        }

        self.pduid_statehash
            .iter()
            .next_back()
            .map(|pair| Ok(pair?.1.to_vec()))
            .ok_or_else(|| Error::bad_database("No PDU's found for this room."))?
    }

    /// This fetches auth event_ids from the current state using the
    /// full `roomstateid_pdu` tree.
    pub fn get_auth_event_ids(
        &self,
        room_id: &RoomId,
        kind: &EventType,
        sender: &UserId,
        state_key: Option<&str>,
        content: serde_json::Value,
    ) -> Result<Vec<EventId>> {
        let auth_events = state_res::auth_types_for_event(
            kind.clone(),
            sender,
            state_key.map(|s| s.to_string()),
            content,
        );

        let mut events = vec![];
        for (event_type, state_key) in auth_events {
            if let Some(state_key) = state_key.as_ref() {
                if let Some(id) = self.room_state_get(room_id, &event_type, state_key)? {
                    events.push(id.event_id);
                }
            }
        }
        Ok(events)
    }

    // This fetches auth events from the current state using the
    /// full `roomstateid_pdu` tree.
    pub fn get_auth_events(
        &self,
        room_id: &RoomId,
        kind: &EventType,
        sender: &UserId,
        state_key: Option<&str>,
        content: serde_json::Value,
    ) -> Result<StateMap<PduEvent>> {
        let auth_events = state_res::auth_types_for_event(
            kind.clone(),
            sender,
            state_key.map(|s| s.to_string()),
            content,
        );

        let mut events = StateMap::new();
        for (event_type, state_key) in auth_events {
            if let Some(s_key) = state_key.as_ref() {
                if let Some(pdu) = self.room_state_get(room_id, &event_type, s_key)? {
                    events.insert((event_type, state_key), pdu);
                }
            }
        }
        Ok(events)
    }

    /// Generate a new StateHash.
    ///
    /// A unique hash made from hashing the current states pduid's.
    /// Because `append_state_pdu` handles the empty state db case it does not
    /// have to be here.
    fn new_state_hash_id(&self, room_id: &RoomId) -> Result<StateHashId> {
        // Use hashed roomId as the first StateHash key for first state event in room
        if self
            .pduid_statehash
            .scan_prefix(room_id.as_bytes())
            .next()
            .is_none()
        {
            return Ok(digest::digest(&digest::SHA256, room_id.as_bytes())
                .as_ref()
                .to_vec());
        }

        let pdu_ids_to_hash = self
            .pduid_statehash
            .scan_prefix(room_id.as_bytes())
            .values()
            .next_back()
            .unwrap() // We just checked if the tree was empty
            .map(|hash| {
                self.stateid_pduid
                    .scan_prefix(hash)
                    .values()
                    // pduid is roomId + count so just hash the whole thing
                    .map(|pid| Ok(pid?.to_vec()))
                    .collect::<Result<Vec<Vec<u8>>>>()
            })??;

        let hash = digest::digest(
            &digest::SHA256,
            &pdu_ids_to_hash.into_iter().flatten().collect::<Vec<u8>>(),
        );
        Ok(hash.as_ref().to_vec())
    }

    /// Checks if a room exists.
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        // Look for PDUs in that room.
        Ok(self
            .pduid_pdu
            .get_gt(&prefix)?
            .filter(|(k, _)| k.starts_with(&prefix))
            .is_some())
    }

    /// Returns the full room state.
    pub fn room_state_full(
        &self,
        room_id: &RoomId,
    ) -> Result<HashMap<(EventType, String), PduEvent>> {
        let mut hashmap = HashMap::new();
        for pdu in
            self.roomstateid_pduid
                .scan_prefix(&room_id.to_string().as_bytes())
                .values()
                .map(|value| {
                    Ok::<_, Error>(
                        serde_json::from_slice::<PduEvent>(
                            &self.pduid_pdu.get(value?)?.ok_or_else(|| {
                                Error::bad_database("PDU not found for ID in db.")
                            })?,
                        )
                        .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                    )
                })
        {
            let pdu = pdu?;
            let state_key = pdu.state_key.clone().ok_or_else(|| {
                Error::bad_database("Room state contains event without state_key.")
            })?;
            hashmap.insert((pdu.kind.clone(), state_key), pdu);
        }
        Ok(hashmap)
    }

    /// Returns the all state entries for this type.
    pub fn room_state_type(
        &self,
        room_id: &RoomId,
        event_type: &EventType,
    ) -> Result<HashMap<String, PduEvent>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&event_type.to_string().as_bytes());

        let mut hashmap = HashMap::new();
        for pdu in
            self.roomstateid_pduid
                .scan_prefix(&prefix)
                .values()
                .map(|value| {
                    Ok::<_, Error>(
                        serde_json::from_slice::<PduEvent>(
                            &self.pduid_pdu.get(value?)?.ok_or_else(|| {
                                Error::bad_database("PDU not found for ID in db.")
                            })?,
                        )
                        .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                    )
                })
        {
            let pdu = pdu?;
            let state_key = pdu.state_key.clone().ok_or_else(|| {
                Error::bad_database("Room state contains event without state_key.")
            })?;
            hashmap.insert(state_key, pdu);
        }
        Ok(hashmap)
    }

    /// Returns a single PDU in `room_id` with key (`event_type`, `state_key`).
    pub fn room_state_get(
        &self,
        room_id: &RoomId,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<PduEvent>> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&event_type.to_string().as_bytes());
        key.push(0xff);
        key.extend_from_slice(&state_key.as_bytes());

        self.roomstateid_pduid.get(&key)?.map_or(Ok(None), |value| {
            Ok::<_, Error>(Some(
                serde_json::from_slice::<PduEvent>(
                    &self
                        .pduid_pdu
                        .get(value)?
                        .ok_or_else(|| Error::bad_database("PDU not found for ID in db."))?,
                )
                .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Returns the `count` of this pdu's id.
    pub fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| {
                Ok(Some(
                    utils::u64_from_bytes(
                        &pdu_id[pdu_id.len() - mem::size_of::<u64>()..pdu_id.len()],
                    )
                    .map_err(|_| Error::bad_database("PDU has invalid count bytes."))?,
                ))
            })
    }

    /// Returns the json of a pdu.
    pub fn get_pdu_json(&self, event_id: &EventId) -> Result<Option<serde_json::Value>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| {
                Ok(Some(
                    serde_json::from_slice(&self.pduid_pdu.get(pdu_id)?.ok_or_else(|| {
                        Error::bad_database("eventid_pduid points to nonexistent pdu.")
                    })?)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                ))
            })
    }

    /// Returns the pdu's id.
    pub fn get_pdu_id(&self, event_id: &EventId) -> Result<Option<IVec>> {
        self.eventid_pduid
            .get(event_id.to_string().as_bytes())?
            .map_or(Ok(None), |pdu_id| Ok(Some(pdu_id)))
    }

    /// Returns the pdu.
    pub fn get_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| {
                Ok(Some(
                    serde_json::from_slice(&self.pduid_pdu.get(pdu_id)?.ok_or_else(|| {
                        Error::bad_database("eventid_pduid points to nonexistent pdu.")
                    })?)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                ))
            })
    }
    /// Returns the pdu.
    pub fn get_pdu_from_id(&self, pdu_id: &IVec) -> Result<Option<PduEvent>> {
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
    pub fn get_pdu_leaves(&self, room_id: &RoomId) -> Result<Vec<EventId>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut events = Vec::new();

        for event in self
            .roomid_pduleaves
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
        {
            events.push(event?);
        }

        Ok(events)
    }

    /// Replace the leaves of a room with a new event.
    pub fn replace_pdu_leaves(&self, room_id: &RoomId, event_id: &EventId) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        for key in self.roomid_pduleaves.scan_prefix(&prefix).keys() {
            self.roomid_pduleaves.remove(key?)?;
        }

        prefix.extend_from_slice(event_id.as_bytes());
        self.roomid_pduleaves.insert(&prefix, event_id.as_bytes())?;

        Ok(())
    }

    /// Creates a new persisted data unit and adds it to a room.
    pub fn append_pdu(
        &self,
        pdu: PduEvent,
        globals: &super::globals::Globals,
        account_data: &super::account_data::AccountData,
    ) -> Result<EventId> {
        let mut pdu_json = serde_json::to_value(&pdu).expect("event is valid, we just created it");
        ruma::signatures::hash_and_sign_event(
            globals.server_name().as_str(),
            globals.keypair(),
            &mut pdu_json,
        )
        .expect("event is valid, we just created it");

        self.replace_pdu_leaves(&pdu.room_id, &pdu.event_id)?;

        // Increment the last index and use that
        // This is also the next_batch/since value
        let index = globals.next_count()?;

        let mut pdu_id = pdu.room_id.as_bytes().to_vec();
        pdu_id.push(0xff);
        pdu_id.extend_from_slice(&index.to_be_bytes());

        self.pduid_pdu.insert(&pdu_id, &*pdu_json.to_string())?;

        self.eventid_pduid
            .insert(pdu.event_id.as_bytes(), &*pdu_id)?;

        if let Some(state_key) = &pdu.state_key {
            self.append_state_pdu(&pdu.room_id, &pdu_id, state_key, &pdu.kind)?;
        }

        match pdu.kind {
            EventType::RoomRedaction => {
                if let Some(redact_id) = &pdu.redacts {
                    // TODO: Reason
                    let _reason = serde_json::from_value::<Raw<redaction::RedactionEventContent>>(
                        pdu.content,
                    )
                    .expect("Raw::from_value always works.")
                    .deserialize()
                    .map_err(|_| {
                        Error::BadRequest(
                            ErrorKind::InvalidParam,
                            "Invalid redaction event content.",
                        )
                    })?
                    .reason;

                    self.redact_pdu(&redact_id)?;
                }
            }
            EventType::RoomMember => {
                if let Some(state_key) = &pdu.state_key {
                    // if the state_key fails
                    let target_user_id = UserId::try_from(state_key.as_str())
                        .expect("This state_key was previously validated");
                    // Update our membership info, we do this here incase a user is invited
                    // and immediately leaves we need the DB to record the invite event for auth
                    self.update_membership(
                        &pdu.room_id,
                        &target_user_id,
                        serde_json::from_value::<member::MemberEventContent>(pdu.content).map_err(
                            |_| {
                                Error::BadRequest(
                                    ErrorKind::InvalidParam,
                                    "Invalid redaction event content.",
                                )
                            },
                        )?,
                        &pdu.sender,
                        account_data,
                        globals,
                    )?;
                }
            }
            _ => {}
        }
        self.edus.room_read_set(&pdu.room_id, &pdu.sender, index)?;

        Ok(pdu.event_id)
    }

    /// Generates a new StateHash and associates it with the incoming event.
    ///
    /// This adds all current state events (not including the incoming event)
    /// to `stateid_pduid` and adds the incoming event to `pduid_statehash`.
    /// The incoming event is the `pdu_id` passed to this method.
    fn append_state_pdu(
        &self,
        room_id: &RoomId,
        pdu_id: &[u8],
        state_key: &str,
        kind: &EventType,
    ) -> Result<StateHashId> {
        let state_hash = self.new_state_hash_id(room_id)?;
        let state = self.current_state_pduids(room_id)?;

        let mut key = state_hash.to_vec();
        key.push(0xff);

        // TODO eventually we could avoid writing to the DB so much on every event
        // by keeping track of the delta and write that every so often
        for ((ev_ty, state_key), pid) in state {
            let mut state_id = key.to_vec();
            state_id.extend_from_slice(ev_ty.to_string().as_bytes());
            key.push(0xff);
            state_id.extend_from_slice(state_key.expect("state event").as_bytes());
            key.push(0xff);

            self.stateid_pduid.insert(&state_id, &pid)?;
        }

        // This event's state does not include the event itself. `current_state_pduids`
        // uses `roomstateid_pduid` before the current event is inserted to the tree so the state
        // will be everything up to but not including the incoming event.
        self.pduid_statehash.insert(pdu_id, state_hash.as_slice())?;

        self.roomid_statehash
            .insert(room_id.as_bytes(), state_hash.as_slice())?;

        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(kind.to_string().as_bytes());
        key.push(0xff);
        key.extend_from_slice(state_key.as_bytes());
        self.roomstateid_pduid.insert(key, pdu_id)?;

        Ok(state_hash)
    }

    /// Creates a new persisted data unit and adds it to a room.
    pub fn build_and_append_pdu(
        &self,
        pdu_builder: PduBuilder,
        globals: &super::globals::Globals,
        account_data: &super::account_data::AccountData,
    ) -> Result<EventId> {
        let PduBuilder {
            room_id,
            sender,
            event_type,
            content,
            unsigned,
            state_key,
            redacts,
        } = pdu_builder;
        // TODO: Make sure this isn't called twice in parallel
        let prev_events = self.get_pdu_leaves(&room_id)?;

        let auth_events = self.get_auth_events(
            &room_id,
            &event_type,
            &sender,
            state_key.as_deref(),
            content.clone(),
        )?;

        // Is the event authorized?
        if let Some(state_key) = &state_key {
            let power_levels = self
                .room_state_get(&room_id, &EventType::RoomPowerLevels, "")?
                .map_or_else(
                    || {
                        Ok::<_, Error>(power_levels::PowerLevelsEventContent {
                            ban: 50.into(),
                            events: BTreeMap::new(),
                            events_default: 0.into(),
                            invite: 50.into(),
                            kick: 50.into(),
                            redact: 50.into(),
                            state_default: 0.into(),
                            users: BTreeMap::new(),
                            users_default: 0.into(),
                            notifications:
                                ruma::events::room::power_levels::NotificationPowerLevels {
                                    room: 50.into(),
                                },
                        })
                    },
                    |power_levels| {
                        Ok(serde_json::from_value::<Raw<PowerLevelsEventContent>>(
                            power_levels.content,
                        )
                        .expect("Raw::from_value always works.")
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid PowerLevels event in db."))?)
                    },
                )?;
            let sender_membership = self
                .room_state_get(&room_id, &EventType::RoomMember, &sender.to_string())?
                .map_or(Ok::<_, Error>(member::MembershipState::Leave), |pdu| {
                    Ok(
                        serde_json::from_value::<Raw<member::MemberEventContent>>(pdu.content)
                            .expect("Raw::from_value always works.")
                            .deserialize()
                            .map_err(|_| Error::bad_database("Invalid Member event in db."))?
                            .membership,
                    )
                })?;

            let sender_power = power_levels.users.get(&sender).map_or_else(
                || {
                    if sender_membership != member::MembershipState::Join {
                        None
                    } else {
                        Some(&power_levels.users_default)
                    }
                },
                // If it's okay, wrap with Some(_)
                Some,
            );

            // Is the event allowed?
            #[allow(clippy::blocks_in_if_conditions)]
            if !match event_type {
                EventType::RoomEncryption => {
                    // Don't allow encryption events when it's disabled
                    !globals.encryption_disabled()
                }
                EventType::RoomMember => event_auth::is_membership_change_allowed(
                    // TODO this is a bit of a hack but not sure how to have a type
                    // declared in `state_res` crate easily convert to/from conduit::PduEvent
                    Requester {
                        prev_event_ids: prev_events.to_owned(),
                        room_id: &room_id,
                        content: &content,
                        state_key: Some(state_key.to_owned()),
                        sender: &sender,
                    },
                    &auth_events
                        .iter()
                        .map(|((ty, key), pdu)| {
                            Ok(((ty.clone(), key.clone()), pdu.convert_for_state_res()?))
                        })
                        .collect::<Result<StateMap<_>>>()?,
                )
                .ok_or(Error::Conflict("Found incoming PDU with invalid data."))?,
                EventType::RoomCreate => prev_events.is_empty(),
                // Not allow any of the following events if the sender is not joined.
                _ if sender_membership != member::MembershipState::Join => false,
                _ => {
                    // TODO
                    sender_power.unwrap_or(&power_levels.users_default)
                        >= &power_levels.state_default
                }
            } {
                error!("Unauthorized {}", event_type);
                // Not authorized
                return Err(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "Event is not authorized",
                ));
            }
        } else if !self.is_joined(&sender, &room_id)? {
            // TODO: auth rules apply to all events, not only those with a state key
            error!("Unauthorized {}", event_type);
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "Event is not authorized",
            ));
        }

        // Our depth is the maximum depth of prev_events + 1
        let depth = prev_events
            .iter()
            .filter_map(|event_id| Some(self.get_pdu_json(event_id).ok()??.get("depth")?.as_u64()?))
            .max()
            .unwrap_or(0_u64)
            + 1;

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
            event_id: EventId::try_from("$thiswillbefilledinlater").expect("we know this is valid"),
            room_id,
            sender,
            origin: globals.server_name().to_owned(),
            origin_server_ts: utils::millis_since_unix_epoch()
                .try_into()
                .expect("time is valid"),
            kind: event_type,
            content,
            state_key,
            prev_events,
            depth: depth
                .try_into()
                .map_err(|_| Error::bad_database("Depth is invalid"))?,
            auth_events: auth_events
                .into_iter()
                .map(|(_, pdu)| pdu.event_id)
                .collect(),
            redacts,
            unsigned,
            hashes: ruma::events::pdu::EventHash {
                sha256: "aaa".to_owned(),
            },
            signatures: HashMap::new(),
        };

        // Generate event id
        pdu.event_id = EventId::try_from(&*format!(
            "${}",
            ruma::signatures::reference_hash(
                &serde_json::to_value(&pdu).expect("event is valid, we just created it")
            )
            .expect("ruma can calculate reference hashes")
        ))
        .expect("ruma's reference hashes are valid event ids");

        self.append_pdu(pdu, globals, account_data)
    }

    /// Returns an iterator over all PDUs in a room.
    pub fn all_pdus(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<impl Iterator<Item = Result<PduEvent>>> {
        self.pdus_since(user_id, room_id, 0)
    }

    /// Returns a double-ended iterator over all events in a room that happened after the event with id `since`
    /// in chronological order.
    pub fn pdus_since(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<impl DoubleEndedIterator<Item = Result<PduEvent>>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
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
            .map(move |(_, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok(pdu)
            }))
    }

    /// Returns an iterator over all events and their tokens in a room that happened before the
    /// event with id `until` in reverse-chronological order.
    pub fn pdus_until(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        until: u64,
    ) -> impl Iterator<Item = Result<(u64, PduEvent)>> {
        // Create the first part of the full pdu id
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut current = prefix.clone();
        current.extend_from_slice(&until.to_be_bytes());

        let current: &[u8] = &current;

        let user_id = user_id.clone();
        let prefixlen = prefix.len();
        self.pduid_pdu
            .range(..current)
            .rev()
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(k, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((
                    utils::u64_from_bytes(&k[prefixlen..])
                        .map_err(|_| Error::bad_database("Invalid pdu id in db."))?,
                    pdu,
                ))
            })
    }

    /// Returns an iterator over all events and their token in a room that happened after the event
    /// with id `from` in chronological order.
    pub fn pdus_after(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        from: u64,
    ) -> impl Iterator<Item = Result<(u64, PduEvent)>> {
        // Create the first part of the full pdu id
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut current = prefix.clone();
        current.extend_from_slice(&(from + 1).to_be_bytes()); // +1 so we don't send the base event

        let current: &[u8] = &current;

        let user_id = user_id.clone();
        let prefixlen = prefix.len();
        self.pduid_pdu
            .range(current..)
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(k, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((
                    utils::u64_from_bytes(&k[prefixlen..])
                        .map_err(|_| Error::bad_database("Invalid pdu id in db."))?,
                    pdu,
                ))
            })
    }

    /// Replace a PDU with the redacted form.
    pub fn redact_pdu(&self, event_id: &EventId) -> Result<()> {
        if let Some(pdu_id) = self.get_pdu_id(event_id)? {
            let mut pdu = self
                .get_pdu_from_id(&pdu_id)?
                .ok_or_else(|| Error::bad_database("PDU ID points to invalid PDU."))?;
            pdu.redact()?;
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
    fn update_membership(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        mut member_content: member::MemberEventContent,
        sender: &UserId,
        account_data: &super::account_data::AccountData,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let membership = member_content.membership;
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        let mut roomuser_id = room_id.to_string().as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.to_string().as_bytes());

        match &membership {
            member::MembershipState::Join => {
                self.userroomid_joined.insert(&userroom_id, &[])?;
                self.roomuserid_joined.insert(&roomuser_id, &[])?;
                self.userroomid_invited.remove(&userroom_id)?;
                self.roomuserid_invited.remove(&roomuser_id)?;
                self.userroomid_left.remove(&userroom_id)?;
            }
            member::MembershipState::Invite => {
                // We want to know if the sender is ignored by the receiver
                let is_ignored = account_data
                    .get::<ignored_user_list::IgnoredUserListEvent>(
                        None,     // Ignored users are in global account data
                        &user_id, // Receiver
                        EventType::IgnoredUserList,
                    )?
                    .map_or(false, |ignored| {
                        ignored.content.ignored_users.contains(&sender)
                    });

                if is_ignored {
                    member_content.membership = member::MembershipState::Leave;

                    self.build_and_append_pdu(
                        PduBuilder {
                            room_id: room_id.clone(),
                            sender: user_id.clone(),
                            event_type: EventType::RoomMember,
                            content: serde_json::to_value(member_content)
                                .expect("event is valid, we just created it"),
                            unsigned: None,
                            state_key: Some(user_id.to_string()),
                            redacts: None,
                        },
                        globals,
                        account_data,
                    )?;

                    return Ok(());
                }
                self.userroomid_invited.insert(&userroom_id, &[])?;
                self.roomuserid_invited.insert(&roomuser_id, &[])?;
                self.userroomid_joined.remove(&userroom_id)?;
                self.roomuserid_joined.remove(&roomuser_id)?;
                self.userroomid_left.remove(&userroom_id)?;
            }
            member::MembershipState::Leave | member::MembershipState::Ban => {
                self.userroomid_left.insert(&userroom_id, &[])?;
                self.userroomid_joined.remove(&userroom_id)?;
                self.roomuserid_joined.remove(&roomuser_id)?;
                self.userroomid_invited.remove(&userroom_id)?;
                self.roomuserid_invited.remove(&roomuser_id)?;
            }
            _ => {}
        }

        Ok(())
    }

    /// Makes a user forget a room.
    pub fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()> {
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        self.userroomid_left.remove(userroom_id)?;

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
                .insert(alias.alias(), &*room_id.to_string())?;
            let mut aliasid = room_id.to_string().as_bytes().to_vec();
            aliasid.extend_from_slice(&globals.next_count()?.to_be_bytes());
            self.aliasid_alias.insert(aliasid, &*alias.alias())?;
        } else {
            // room_id=None means remove alias
            let room_id = self
                .alias_roomid
                .remove(alias.alias())?
                .ok_or(Error::BadRequest(
                    ErrorKind::NotFound,
                    "Alias does not exist.",
                ))?;

            for key in self.aliasid_alias.scan_prefix(room_id).keys() {
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
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        self.aliasid_alias
            .scan_prefix(prefix)
            .values()
            .map(|bytes| {
                Ok(serde_json::from_slice(&bytes?)
                    .map_err(|_| Error::bad_database("Alias in aliasid_alias is invalid."))?)
            })
    }

    pub fn set_public(&self, room_id: &RoomId, public: bool) -> Result<()> {
        if public {
            self.publicroomids.insert(room_id.to_string(), &[])?;
        } else {
            self.publicroomids.remove(room_id.to_string())?;
        }

        Ok(())
    }

    pub fn is_public_room(&self, room_id: &RoomId) -> Result<bool> {
        Ok(self.publicroomids.contains_key(room_id.to_string())?)
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

    pub fn search_pdus(
        &self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<(impl Iterator<Item = IVec>, Vec<String>)> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let words = search_string
            .split_terminator(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .collect::<Vec<_>>();

        let mut iterators = words.iter().map(|word| {
            let mut prefix2 = prefix.clone();
            prefix2.extend_from_slice(word.as_bytes());
            prefix2.push(0xff);
            self.tokenids
                .scan_prefix(&prefix2)
                .keys()
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

                    let pdu_id = key.subslice(pduid_index, key.len() - pduid_index);

                    Ok::<_, Error>(pdu_id)
                })
                .filter_map(|r| r.ok())
                .peekable()
        });

        let first_iterator = match iterators.next() {
            Some(i) => i,
            None => {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "search_term needs to contain at least one word.",
                ))
            }
        };

        let mut other_iterators = iterators.collect::<Vec<_>>();

        Ok((
            first_iterator.filter(move |target| {
                other_iterators
                    .iter_mut()
                    .map(|it| {
                        while let Some(element) = it.peek() {
                            if dbg!(element) > dbg!(target) {
                                return false;
                            } else if element == target {
                                return true;
                            } else {
                                it.next();
                            }
                        }

                        false
                    })
                    .all(|b| b)
            }),
            words,
        ))
    }

    /// Returns an iterator over all joined members of a room.
    pub fn room_members(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        self.roomuserid_joined
            .scan_prefix(room_id.to_string())
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

    /// Returns an iterator over all invited members of a room.
    pub fn room_members_invited(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        self.roomuserid_invited
            .scan_prefix(room_id.to_string())
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

    /// Returns an iterator over all rooms this user joined.
    pub fn rooms_joined(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        self.userroomid_joined
            .scan_prefix(user_id.to_string())
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
    pub fn rooms_invited(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        self.userroomid_invited
            .scan_prefix(&user_id.to_string())
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
                        Error::bad_database("Room ID in userroomid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_invited is invalid."))?)
            })
    }

    /// Returns an iterator over all rooms a user left.
    pub fn rooms_left(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        self.userroomid_left
            .scan_prefix(&user_id.to_string())
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
                        Error::bad_database("Room ID in userroomid_left is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_left is invalid."))?)
            })
    }

    pub fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        Ok(self.userroomid_joined.get(userroom_id)?.is_some())
    }

    pub fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        Ok(self.userroomid_invited.get(userroom_id)?.is_some())
    }

    pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        Ok(self.userroomid_left.get(userroom_id)?.is_some())
    }
}
