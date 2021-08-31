mod edus;

pub use edus::RoomEdus;
use member::MembershipState;

use crate::{pdu::PduBuilder, server_server, utils, Database, Error, PduEvent, Result};
use lru_cache::LruCache;
use regex::Regex;
use ring::digest;
use rocket::http::RawStr;
use ruma::{
    api::{client::error::ErrorKind, federation},
    events::{
        ignored_user_list, push_rules,
        room::{
            create::CreateEventContent, member, message, power_levels::PowerLevelsEventContent,
        },
        AnyStrippedStateEvent, AnySyncStateEvent, EventType,
    },
    push::{self, Action, Tweak},
    serde::{CanonicalJsonObject, CanonicalJsonValue, Raw},
    state_res::{self, RoomVersion, StateMap},
    uint, EventId, RoomAliasId, RoomId, RoomVersionId, ServerName, UserId,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    mem::size_of,
    sync::{Arc, Mutex, RwLock},
    time::Instant,
};
use tokio::sync::MutexGuard;
use tracing::{error, warn};

use super::{abstraction::Tree, admin::AdminCommand, pusher};

/// The unique identifier of each state group.
///
/// This is created when a state group is added to the database by
/// hashing the entire state.
pub type StateHashId = Vec<u8>;
pub type CompressedStateEvent = [u8; 2 * size_of::<u64>()];

pub struct Rooms {
    pub edus: edus::RoomEdus,
    pub(super) pduid_pdu: Arc<dyn Tree>, // PduId = ShortRoomId + Count
    pub(super) eventid_pduid: Arc<dyn Tree>,
    pub(super) roomid_pduleaves: Arc<dyn Tree>,
    pub(super) alias_roomid: Arc<dyn Tree>,
    pub(super) aliasid_alias: Arc<dyn Tree>, // AliasId = RoomId + Count
    pub(super) publicroomids: Arc<dyn Tree>,

    pub(super) tokenids: Arc<dyn Tree>, // TokenId = ShortRoomId + Token + PduIdCount

    /// Participating servers in a room.
    pub(super) roomserverids: Arc<dyn Tree>, // RoomServerId = RoomId + ServerName
    pub(super) serverroomids: Arc<dyn Tree>, // ServerRoomId = ServerName + RoomId

    pub(super) userroomid_joined: Arc<dyn Tree>,
    pub(super) roomuserid_joined: Arc<dyn Tree>,
    pub(super) roomid_joinedcount: Arc<dyn Tree>,
    pub(super) roomid_invitedcount: Arc<dyn Tree>,
    pub(super) roomuseroncejoinedids: Arc<dyn Tree>,
    pub(super) userroomid_invitestate: Arc<dyn Tree>, // InviteState = Vec<Raw<Pdu>>
    pub(super) roomuserid_invitecount: Arc<dyn Tree>, // InviteCount = Count
    pub(super) userroomid_leftstate: Arc<dyn Tree>,
    pub(super) roomuserid_leftcount: Arc<dyn Tree>,

    pub(super) userroomid_notificationcount: Arc<dyn Tree>, // NotifyCount = u64
    pub(super) userroomid_highlightcount: Arc<dyn Tree>,    // HightlightCount = u64

    /// Remember the current state hash of a room.
    pub(super) roomid_shortstatehash: Arc<dyn Tree>,
    pub(super) roomsynctoken_shortstatehash: Arc<dyn Tree>,
    /// Remember the state hash at events in the past.
    pub(super) shorteventid_shortstatehash: Arc<dyn Tree>,
    /// StateKey = EventType + StateKey, ShortStateKey = Count
    pub(super) statekey_shortstatekey: Arc<dyn Tree>,
    pub(super) shortstatekey_statekey: Arc<dyn Tree>,

    pub(super) roomid_shortroomid: Arc<dyn Tree>,

    pub(super) shorteventid_eventid: Arc<dyn Tree>,
    pub(super) eventid_shorteventid: Arc<dyn Tree>,

    pub(super) statehash_shortstatehash: Arc<dyn Tree>,
    pub(super) shortstatehash_statediff: Arc<dyn Tree>, // StateDiff = parent (or 0) + (shortstatekey+shorteventid++) + 0_u64 + (shortstatekey+shorteventid--)

    pub(super) shorteventid_authchain: Arc<dyn Tree>,

    /// RoomId + EventId -> outlier PDU.
    /// Any pdu that has passed the steps 1-8 in the incoming event /federation/send/txn.
    pub(super) eventid_outlierpdu: Arc<dyn Tree>,
    pub(super) softfailedeventids: Arc<dyn Tree>,

    /// RoomId + EventId -> Parent PDU EventId.
    pub(super) referencedevents: Arc<dyn Tree>,

    pub(super) pdu_cache: Mutex<LruCache<EventId, Arc<PduEvent>>>,
    pub(super) shorteventid_cache: Mutex<LruCache<u64, Arc<EventId>>>,
    pub(super) auth_chain_cache: Mutex<LruCache<Vec<u64>, Arc<HashSet<u64>>>>,
    pub(super) eventidshort_cache: Mutex<LruCache<EventId, u64>>,
    pub(super) statekeyshort_cache: Mutex<LruCache<(EventType, String), u64>>,
    pub(super) shortstatekey_cache: Mutex<LruCache<u64, (EventType, String)>>,
    pub(super) our_real_users_cache: RwLock<HashMap<RoomId, Arc<HashSet<UserId>>>>,
    pub(super) appservice_in_room_cache: RwLock<HashMap<RoomId, HashMap<String, bool>>>,
    pub(super) stateinfo_cache: Mutex<
        LruCache<
            u64,
            Vec<(
                u64,                           // sstatehash
                HashSet<CompressedStateEvent>, // full state
                HashSet<CompressedStateEvent>, // added
                HashSet<CompressedStateEvent>, // removed
            )>,
        >,
    >,
}

impl Rooms {
    /// Builds a StateMap by iterating over all keys that start
    /// with state_hash, this gives the full state for the given state_hash.
    #[tracing::instrument(skip(self))]
    pub fn state_full_ids(&self, shortstatehash: u64) -> Result<BTreeMap<u64, Arc<EventId>>> {
        let full_state = self
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;
        full_state
            .into_iter()
            .map(|compressed| self.parse_compressed_state_event(compressed))
            .collect()
    }

    #[tracing::instrument(skip(self))]
    pub fn state_full(
        &self,
        shortstatehash: u64,
    ) -> Result<HashMap<(EventType, String), Arc<PduEvent>>> {
        let full_state = self
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;
        Ok(full_state
            .into_iter()
            .map(|compressed| self.parse_compressed_state_event(compressed))
            .filter_map(|r| r.ok())
            .map(|(_, eventid)| self.get_pdu(&eventid))
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
    ) -> Result<Option<Arc<EventId>>> {
        let shortstatekey = match self.get_shortstatekey(event_type, state_key)? {
            Some(s) => s,
            None => return Ok(None),
        };
        let full_state = self
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;
        Ok(full_state
            .into_iter()
            .find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
            .and_then(|compressed| {
                self.parse_compressed_state_event(compressed)
                    .ok()
                    .map(|(_, id)| id)
            }))
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn state_get(
        &self,
        shortstatehash: u64,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<Arc<PduEvent>>> {
        self.state_get_id(shortstatehash, event_type, state_key)?
            .map_or(Ok(None), |event_id| self.get_pdu(&event_id))
    }

    /// Returns the state hash for this pdu.
    #[tracing::instrument(skip(self))]
    pub fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_shorteventid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |shorteventid| {
                self.shorteventid_shortstatehash.get(&shorteventid)?.map_or(
                    Ok::<_, Error>(None),
                    |bytes| {
                        Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                            Error::bad_database(
                                "Invalid shortstatehash bytes in shorteventid_shortstatehash",
                            )
                        })?))
                    },
                )
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
    #[tracing::instrument(skip(self))]
    pub fn get_auth_events(
        &self,
        room_id: &RoomId,
        kind: &EventType,
        sender: &UserId,
        state_key: Option<&str>,
        content: &serde_json::Value,
    ) -> Result<StateMap<Arc<PduEvent>>> {
        let shortstatehash =
            if let Some(current_shortstatehash) = self.current_shortstatehash(room_id)? {
                current_shortstatehash
            } else {
                return Ok(HashMap::new());
            };

        let auth_events = state_res::auth_types_for_event(
            kind,
            sender,
            state_key.map(|s| s.to_string()),
            content.clone(),
        );

        let mut sauthevents = auth_events
            .into_iter()
            .filter_map(|(event_type, state_key)| {
                self.get_shortstatekey(&event_type, &state_key)
                    .ok()
                    .flatten()
                    .map(|s| (s, (event_type, state_key)))
            })
            .collect::<HashMap<_, _>>();

        let full_state = self
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;

        Ok(full_state
            .into_iter()
            .filter_map(|compressed| self.parse_compressed_state_event(compressed).ok())
            .filter_map(|(shortstatekey, event_id)| {
                sauthevents.remove(&shortstatekey).map(|k| (k, event_id))
            })
            .filter_map(|(k, event_id)| self.get_pdu(&event_id).ok().flatten().map(|pdu| (k, pdu)))
            .collect())
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
    #[tracing::instrument(skip(self))]
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        let prefix = match self.get_shortroomid(room_id)? {
            Some(b) => b.to_be_bytes().to_vec(),
            None => return Ok(false),
        };

        // Look for PDUs in that room.
        Ok(self
            .pduid_pdu
            .iter_from(&prefix, false)
            .next()
            .filter(|(k, _)| k.starts_with(&prefix))
            .is_some())
    }

    /// Checks if a room exists.
    #[tracing::instrument(skip(self))]
    pub fn first_pdu_in_room(&self, room_id: &RoomId) -> Result<Option<Arc<PduEvent>>> {
        let prefix = self
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();

        // Look for PDUs in that room.
        self.pduid_pdu
            .iter_from(&prefix, false)
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, pdu)| {
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid first PDU in db."))
                    .map(Arc::new)
            })
            .next()
            .transpose()
    }

    /// Force the creation of a new StateHash and insert it into the db.
    ///
    /// Whatever `state` is supplied to `force_state` becomes the new current room state snapshot.
    #[tracing::instrument(skip(self, new_state_ids_compressed, db))]
    pub fn force_state(
        &self,
        room_id: &RoomId,
        new_state_ids_compressed: HashSet<CompressedStateEvent>,
        db: &Database,
    ) -> Result<()> {
        let previous_shortstatehash = self.current_shortstatehash(&room_id)?;

        let state_hash = self.calculate_hash(
            &new_state_ids_compressed
                .iter()
                .map(|bytes| &bytes[..])
                .collect::<Vec<_>>(),
        );

        let (new_shortstatehash, already_existed) =
            self.get_or_create_shortstatehash(&state_hash, &db.globals)?;

        if Some(new_shortstatehash) == previous_shortstatehash {
            return Ok(());
        }

        let states_parents = previous_shortstatehash
            .map_or_else(|| Ok(Vec::new()), |p| self.load_shortstatehash_info(p))?;

        let (statediffnew, statediffremoved) = if let Some(parent_stateinfo) = states_parents.last()
        {
            let statediffnew = new_state_ids_compressed
                .difference(&parent_stateinfo.1)
                .cloned()
                .collect::<HashSet<_>>();

            let statediffremoved = parent_stateinfo
                .1
                .difference(&new_state_ids_compressed)
                .cloned()
                .collect::<HashSet<_>>();

            (statediffnew, statediffremoved)
        } else {
            (new_state_ids_compressed, HashSet::new())
        };

        if !already_existed {
            self.save_state_from_diff(
                new_shortstatehash,
                statediffnew.clone(),
                statediffremoved.clone(),
                2, // every state change is 2 event changes on average
                states_parents,
            )?;
        };

        for event_id in statediffnew.into_iter().filter_map(|new| {
            self.parse_compressed_state_event(new)
                .ok()
                .map(|(_, id)| id)
        }) {
            if let Some(pdu) = self.get_pdu_json(&event_id)? {
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
                                    false,
                                )?;
                            }
                        }
                    }
                }
            }
        }

        self.update_joined_count(room_id, &db)?;

        self.roomid_shortstatehash
            .insert(room_id.as_bytes(), &new_shortstatehash.to_be_bytes())?;

        Ok(())
    }

    /// Returns a stack with info on shortstatehash, full state, added diff and removed diff for the selected shortstatehash and each parent layer.
    #[tracing::instrument(skip(self))]
    pub fn load_shortstatehash_info(
        &self,
        shortstatehash: u64,
    ) -> Result<
        Vec<(
            u64,                           // sstatehash
            HashSet<CompressedStateEvent>, // full state
            HashSet<CompressedStateEvent>, // added
            HashSet<CompressedStateEvent>, // removed
        )>,
    > {
        if let Some(r) = self
            .stateinfo_cache
            .lock()
            .unwrap()
            .get_mut(&shortstatehash)
        {
            return Ok(r.clone());
        }

        let value = self
            .shortstatehash_statediff
            .get(&shortstatehash.to_be_bytes())?
            .ok_or_else(|| Error::bad_database("State hash does not exist"))?;
        let parent =
            utils::u64_from_bytes(&value[0..size_of::<u64>()]).expect("bytes have right length");

        let mut add_mode = true;
        let mut added = HashSet::new();
        let mut removed = HashSet::new();

        let mut i = size_of::<u64>();
        while let Some(v) = value.get(i..i + 2 * size_of::<u64>()) {
            if add_mode && v.starts_with(&0_u64.to_be_bytes()) {
                add_mode = false;
                i += size_of::<u64>();
                continue;
            }
            if add_mode {
                added.insert(v.try_into().expect("we checked the size above"));
            } else {
                removed.insert(v.try_into().expect("we checked the size above"));
            }
            i += 2 * size_of::<u64>();
        }

        if parent != 0_u64 {
            let mut response = self.load_shortstatehash_info(parent)?;
            let mut state = response.last().unwrap().1.clone();
            state.extend(added.iter().cloned());
            for r in &removed {
                state.remove(r);
            }

            response.push((shortstatehash, state, added, removed));

            Ok(response)
        } else {
            let mut response = Vec::new();
            response.push((shortstatehash, added.clone(), added, removed));
            self.stateinfo_cache
                .lock()
                .unwrap()
                .insert(shortstatehash, response.clone());
            Ok(response)
        }
    }

    #[tracing::instrument(skip(self, globals))]
    pub fn compress_state_event(
        &self,
        shortstatekey: u64,
        event_id: &EventId,
        globals: &super::globals::Globals,
    ) -> Result<CompressedStateEvent> {
        let mut v = shortstatekey.to_be_bytes().to_vec();
        v.extend_from_slice(
            &self
                .get_or_create_shorteventid(event_id, globals)?
                .to_be_bytes(),
        );
        Ok(v.try_into().expect("we checked the size above"))
    }

    /// Returns shortstatekey, event id
    #[tracing::instrument(skip(self, compressed_event))]
    pub fn parse_compressed_state_event(
        &self,
        compressed_event: CompressedStateEvent,
    ) -> Result<(u64, Arc<EventId>)> {
        Ok((
            utils::u64_from_bytes(&compressed_event[0..size_of::<u64>()])
                .expect("bytes have right length"),
            self.get_eventid_from_short(
                utils::u64_from_bytes(&compressed_event[size_of::<u64>()..])
                    .expect("bytes have right length"),
            )?,
        ))
    }

    /// Creates a new shortstatehash that often is just a diff to an already existing
    /// shortstatehash and therefore very efficient.
    ///
    /// There are multiple layers of diffs. The bottom layer 0 always contains the full state. Layer
    /// 1 contains diffs to states of layer 0, layer 2 diffs to layer 1 and so on. If layer n > 0
    /// grows too big, it will be combined with layer n-1 to create a new diff on layer n-1 that's
    /// based on layer n-2. If that layer is also too big, it will recursively fix above layers too.
    ///
    /// * `shortstatehash` - Shortstatehash of this state
    /// * `statediffnew` - Added to base. Each vec is shortstatekey+shorteventid
    /// * `statediffremoved` - Removed from base. Each vec is shortstatekey+shorteventid
    /// * `diff_to_sibling` - Approximately how much the diff grows each time for this layer
    /// * `parent_states` - A stack with info on shortstatehash, full state, added diff and removed diff for each parent layer
    #[tracing::instrument(skip(
        self,
        statediffnew,
        statediffremoved,
        diff_to_sibling,
        parent_states
    ))]
    pub fn save_state_from_diff(
        &self,
        shortstatehash: u64,
        statediffnew: HashSet<CompressedStateEvent>,
        statediffremoved: HashSet<CompressedStateEvent>,
        diff_to_sibling: usize,
        mut parent_states: Vec<(
            u64,                           // sstatehash
            HashSet<CompressedStateEvent>, // full state
            HashSet<CompressedStateEvent>, // added
            HashSet<CompressedStateEvent>, // removed
        )>,
    ) -> Result<()> {
        let diffsum = statediffnew.len() + statediffremoved.len();

        if parent_states.len() > 3 {
            // Number of layers
            // To many layers, we have to go deeper
            let parent = parent_states.pop().unwrap();

            let mut parent_new = parent.2;
            let mut parent_removed = parent.3;

            for removed in statediffremoved {
                if !parent_new.remove(&removed) {
                    // It was not added in the parent and we removed it
                    parent_removed.insert(removed);
                }
                // Else it was added in the parent and we removed it again. We can forget this change
            }

            for new in statediffnew {
                if !parent_removed.remove(&new) {
                    // It was not touched in the parent and we added it
                    parent_new.insert(new);
                }
                // Else it was removed in the parent and we added it again. We can forget this change
            }

            self.save_state_from_diff(
                shortstatehash,
                parent_new,
                parent_removed,
                diffsum,
                parent_states,
            )?;

            return Ok(());
        }

        if parent_states.len() == 0 {
            // There is no parent layer, create a new state
            let mut value = 0_u64.to_be_bytes().to_vec(); // 0 means no parent
            for new in &statediffnew {
                value.extend_from_slice(&new[..]);
            }

            if !statediffremoved.is_empty() {
                warn!("Tried to create new state with removals");
            }

            self.shortstatehash_statediff
                .insert(&shortstatehash.to_be_bytes(), &value)?;

            return Ok(());
        };

        // Else we have two options.
        // 1. We add the current diff on top of the parent layer.
        // 2. We replace a layer above

        let parent = parent_states.pop().unwrap();
        let parent_diff = parent.2.len() + parent.3.len();

        if diffsum * diffsum >= 2 * diff_to_sibling * parent_diff {
            // Diff too big, we replace above layer(s)
            let mut parent_new = parent.2;
            let mut parent_removed = parent.3;

            for removed in statediffremoved {
                if !parent_new.remove(&removed) {
                    // It was not added in the parent and we removed it
                    parent_removed.insert(removed);
                }
                // Else it was added in the parent and we removed it again. We can forget this change
            }

            for new in statediffnew {
                if !parent_removed.remove(&new) {
                    // It was not touched in the parent and we added it
                    parent_new.insert(new);
                }
                // Else it was removed in the parent and we added it again. We can forget this change
            }

            self.save_state_from_diff(
                shortstatehash,
                parent_new,
                parent_removed,
                diffsum,
                parent_states,
            )?;
        } else {
            // Diff small enough, we add diff as layer on top of parent
            let mut value = parent.0.to_be_bytes().to_vec();
            for new in &statediffnew {
                value.extend_from_slice(&new[..]);
            }

            if !statediffremoved.is_empty() {
                value.extend_from_slice(&0_u64.to_be_bytes());
                for removed in &statediffremoved {
                    value.extend_from_slice(&removed[..]);
                }
            }

            self.shortstatehash_statediff
                .insert(&shortstatehash.to_be_bytes(), &value)?;
        }

        Ok(())
    }

    /// Returns (shortstatehash, already_existed)
    #[tracing::instrument(skip(self, globals))]
    fn get_or_create_shortstatehash(
        &self,
        state_hash: &StateHashId,
        globals: &super::globals::Globals,
    ) -> Result<(u64, bool)> {
        Ok(match self.statehash_shortstatehash.get(&state_hash)? {
            Some(shortstatehash) => (
                utils::u64_from_bytes(&shortstatehash)
                    .map_err(|_| Error::bad_database("Invalid shortstatehash in db."))?,
                true,
            ),
            None => {
                let shortstatehash = globals.next_count()?;
                self.statehash_shortstatehash
                    .insert(&state_hash, &shortstatehash.to_be_bytes())?;
                (shortstatehash, false)
            }
        })
    }

    #[tracing::instrument(skip(self, globals))]
    pub fn get_or_create_shorteventid(
        &self,
        event_id: &EventId,
        globals: &super::globals::Globals,
    ) -> Result<u64> {
        if let Some(short) = self.eventidshort_cache.lock().unwrap().get_mut(&event_id) {
            return Ok(*short);
        }

        let short = match self.eventid_shorteventid.get(event_id.as_bytes())? {
            Some(shorteventid) => utils::u64_from_bytes(&shorteventid)
                .map_err(|_| Error::bad_database("Invalid shorteventid in db."))?,
            None => {
                let shorteventid = globals.next_count()?;
                self.eventid_shorteventid
                    .insert(event_id.as_bytes(), &shorteventid.to_be_bytes())?;
                self.shorteventid_eventid
                    .insert(&shorteventid.to_be_bytes(), event_id.as_bytes())?;
                shorteventid
            }
        };

        self.eventidshort_cache
            .lock()
            .unwrap()
            .insert(event_id.clone(), short);

        Ok(short)
    }

    #[tracing::instrument(skip(self))]
    pub fn get_shortroomid(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.roomid_shortroomid
            .get(&room_id.as_bytes())?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid shortroomid in db."))
            })
            .transpose()
    }

    #[tracing::instrument(skip(self))]
    pub fn get_shortstatekey(
        &self,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<u64>> {
        if let Some(short) = self
            .statekeyshort_cache
            .lock()
            .unwrap()
            .get_mut(&(event_type.clone(), state_key.to_owned()))
        {
            return Ok(Some(*short));
        }

        let mut statekey = event_type.as_ref().as_bytes().to_vec();
        statekey.push(0xff);
        statekey.extend_from_slice(&state_key.as_bytes());

        let short = self
            .statekey_shortstatekey
            .get(&statekey)?
            .map(|shortstatekey| {
                utils::u64_from_bytes(&shortstatekey)
                    .map_err(|_| Error::bad_database("Invalid shortstatekey in db."))
            })
            .transpose()?;

        if let Some(s) = short {
            self.statekeyshort_cache
                .lock()
                .unwrap()
                .insert((event_type.clone(), state_key.to_owned()), s);
        }

        Ok(short)
    }

    #[tracing::instrument(skip(self, globals))]
    pub fn get_or_create_shortroomid(
        &self,
        room_id: &RoomId,
        globals: &super::globals::Globals,
    ) -> Result<u64> {
        Ok(match self.roomid_shortroomid.get(&room_id.as_bytes())? {
            Some(short) => utils::u64_from_bytes(&short)
                .map_err(|_| Error::bad_database("Invalid shortroomid in db."))?,
            None => {
                let short = globals.next_count()?;
                self.roomid_shortroomid
                    .insert(&room_id.as_bytes(), &short.to_be_bytes())?;
                short
            }
        })
    }

    #[tracing::instrument(skip(self, globals))]
    pub fn get_or_create_shortstatekey(
        &self,
        event_type: &EventType,
        state_key: &str,
        globals: &super::globals::Globals,
    ) -> Result<u64> {
        if let Some(short) = self
            .statekeyshort_cache
            .lock()
            .unwrap()
            .get_mut(&(event_type.clone(), state_key.to_owned()))
        {
            return Ok(*short);
        }

        let mut statekey = event_type.as_ref().as_bytes().to_vec();
        statekey.push(0xff);
        statekey.extend_from_slice(&state_key.as_bytes());

        let short = match self.statekey_shortstatekey.get(&statekey)? {
            Some(shortstatekey) => utils::u64_from_bytes(&shortstatekey)
                .map_err(|_| Error::bad_database("Invalid shortstatekey in db."))?,
            None => {
                let shortstatekey = globals.next_count()?;
                self.statekey_shortstatekey
                    .insert(&statekey, &shortstatekey.to_be_bytes())?;
                self.shortstatekey_statekey
                    .insert(&shortstatekey.to_be_bytes(), &statekey)?;
                shortstatekey
            }
        };

        self.statekeyshort_cache
            .lock()
            .unwrap()
            .insert((event_type.clone(), state_key.to_owned()), short);

        Ok(short)
    }

    #[tracing::instrument(skip(self))]
    pub fn get_eventid_from_short(&self, shorteventid: u64) -> Result<Arc<EventId>> {
        if let Some(id) = self
            .shorteventid_cache
            .lock()
            .unwrap()
            .get_mut(&shorteventid)
        {
            return Ok(Arc::clone(id));
        }

        let bytes = self
            .shorteventid_eventid
            .get(&shorteventid.to_be_bytes())?
            .ok_or_else(|| Error::bad_database("Shorteventid does not exist"))?;

        let event_id = Arc::new(
            EventId::try_from(utils::string_from_bytes(&bytes).map_err(|_| {
                Error::bad_database("EventID in shorteventid_eventid is invalid unicode.")
            })?)
            .map_err(|_| Error::bad_database("EventId in shorteventid_eventid is invalid."))?,
        );

        self.shorteventid_cache
            .lock()
            .unwrap()
            .insert(shorteventid, Arc::clone(&event_id));

        Ok(event_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn get_statekey_from_short(&self, shortstatekey: u64) -> Result<(EventType, String)> {
        if let Some(id) = self
            .shortstatekey_cache
            .lock()
            .unwrap()
            .get_mut(&shortstatekey)
        {
            return Ok(id.clone());
        }

        let bytes = self
            .shortstatekey_statekey
            .get(&shortstatekey.to_be_bytes())?
            .ok_or_else(|| Error::bad_database("Shortstatekey does not exist"))?;

        let mut parts = bytes.splitn(2, |&b| b == 0xff);
        let eventtype_bytes = parts.next().expect("split always returns one entry");
        let statekey_bytes = parts
            .next()
            .ok_or_else(|| Error::bad_database("Invalid statekey in shortstatekey_statekey."))?;

        let event_type =
            EventType::try_from(utils::string_from_bytes(&eventtype_bytes).map_err(|_| {
                Error::bad_database("Event type in shortstatekey_statekey is invalid unicode.")
            })?)
            .map_err(|_| Error::bad_database("Event type in shortstatekey_statekey is invalid."))?;

        let state_key = utils::string_from_bytes(&statekey_bytes).map_err(|_| {
            Error::bad_database("Statekey in shortstatekey_statekey is invalid unicode.")
        })?;

        let result = (event_type, state_key);

        self.shortstatekey_cache
            .lock()
            .unwrap()
            .insert(shortstatekey, result.clone());

        Ok(result)
    }

    /// Returns the full room state.
    #[tracing::instrument(skip(self))]
    pub fn room_state_full(
        &self,
        room_id: &RoomId,
    ) -> Result<HashMap<(EventType, String), Arc<PduEvent>>> {
        if let Some(current_shortstatehash) = self.current_shortstatehash(room_id)? {
            self.state_full(current_shortstatehash)
        } else {
            Ok(HashMap::new())
        }
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn room_state_get_id(
        &self,
        room_id: &RoomId,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<Arc<EventId>>> {
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
    ) -> Result<Option<Arc<PduEvent>>> {
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
            utils::u64_from_bytes(&pdu_id[pdu_id.len() - size_of::<u64>()..])
                .map_err(|_| Error::bad_database("PDU has invalid count bytes."))?,
        )
    }

    /// Returns the `count` of this pdu's id.
    #[tracing::instrument(skip(self))]
    pub fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| self.pdu_count(&pdu_id).map(Some))
    }

    #[tracing::instrument(skip(self))]
    pub fn latest_pdu_count(&self, room_id: &RoomId) -> Result<u64> {
        let prefix = self
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();

        let mut last_possible_key = prefix.clone();
        last_possible_key.extend_from_slice(&u64::MAX.to_be_bytes());

        self.pduid_pdu
            .iter_from(&last_possible_key, true)
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .next()
            .map(|b| self.pdu_count(&b.0))
            .transpose()
            .map(|op| op.unwrap_or_default())
    }

    /// Returns the json of a pdu.
    #[tracing::instrument(skip(self))]
    pub fn get_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or_else::<Result<_>, _, _>(
                || self.eventid_outlierpdu.get(event_id.as_bytes()),
                |pduid| {
                    Ok(Some(self.pduid_pdu.get(&pduid)?.ok_or_else(|| {
                        Error::bad_database("Invalid pduid in eventid_pduid.")
                    })?))
                },
            )?
            .map(|pdu| {
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
            .transpose()
    }

    /// Returns the json of a pdu.
    #[tracing::instrument(skip(self))]
    pub fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>> {
        self.eventid_outlierpdu
            .get(event_id.as_bytes())?
            .map(|pdu| {
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
            .transpose()
    }

    /// Returns the json of a pdu.
    #[tracing::instrument(skip(self))]
    pub fn get_non_outlier_pdu_json(
        &self,
        event_id: &EventId,
    ) -> Result<Option<CanonicalJsonObject>> {
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
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
            .transpose()
    }

    /// Returns the pdu's id.
    #[tracing::instrument(skip(self))]
    pub fn get_pdu_id(&self, event_id: &EventId) -> Result<Option<Vec<u8>>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu_id| Ok(Some(pdu_id)))
    }

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    #[tracing::instrument(skip(self))]
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
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
            .transpose()
    }

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    #[tracing::instrument(skip(self))]
    pub fn get_pdu(&self, event_id: &EventId) -> Result<Option<Arc<PduEvent>>> {
        if let Some(p) = self.pdu_cache.lock().unwrap().get_mut(&event_id) {
            return Ok(Some(Arc::clone(p)));
        }

        if let Some(pdu) = self
            .eventid_pduid
            .get(event_id.as_bytes())?
            .map_or_else::<Result<_>, _, _>(
                || {
                    let r = self.eventid_outlierpdu.get(event_id.as_bytes());
                    r
                },
                |pduid| {
                    Ok(Some(self.pduid_pdu.get(&pduid)?.ok_or_else(|| {
                        Error::bad_database("Invalid pduid in eventid_pduid.")
                    })?))
                },
            )?
            .map(|pdu| {
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))
                    .map(Arc::new)
            })
            .transpose()?
        {
            self.pdu_cache
                .lock()
                .unwrap()
                .insert(event_id.clone(), Arc::clone(&pdu));
            Ok(Some(pdu))
        } else {
            Ok(None)
        }
    }

    /// Returns the pdu.
    ///
    /// This does __NOT__ check the outliers `Tree`.
    #[tracing::instrument(skip(self))]
    pub fn get_pdu_from_id(&self, pdu_id: &[u8]) -> Result<Option<PduEvent>> {
        self.pduid_pdu.get(pdu_id)?.map_or(Ok(None), |pdu| {
            Ok(Some(
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
    #[tracing::instrument(skip(self))]
    pub fn get_pdu_json_from_id(&self, pdu_id: &[u8]) -> Result<Option<CanonicalJsonObject>> {
        self.pduid_pdu.get(pdu_id)?.map_or(Ok(None), |pdu| {
            Ok(Some(
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Removes a pdu and creates a new one with the same id.
    #[tracing::instrument(skip(self))]
    fn replace_pdu(&self, pdu_id: &[u8], pdu: &PduEvent) -> Result<()> {
        if self.pduid_pdu.get(&pdu_id)?.is_some() {
            self.pduid_pdu.insert(
                &pdu_id,
                &serde_json::to_vec(pdu).expect("PduEvent::to_vec always works"),
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
    #[tracing::instrument(skip(self))]
    pub fn get_pdu_leaves(&self, room_id: &RoomId) -> Result<HashSet<EventId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomid_pduleaves
            .scan_prefix(prefix)
            .map(|(_, bytes)| {
                EventId::try_from(utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("EventID in roomid_pduleaves is invalid unicode.")
                })?)
                .map_err(|_| Error::bad_database("EventId in roomid_pduleaves is invalid."))
            })
            .collect()
    }

    #[tracing::instrument(skip(self, room_id, event_ids))]
    pub fn mark_as_referenced(&self, room_id: &RoomId, event_ids: &[EventId]) -> Result<()> {
        for prev in event_ids {
            let mut key = room_id.as_bytes().to_vec();
            key.extend_from_slice(prev.as_bytes());
            self.referencedevents.insert(&key, &[])?;
        }

        Ok(())
    }

    /// Replace the leaves of a room.
    ///
    /// The provided `event_ids` become the new leaves, this allows a room to have multiple
    /// `prev_events`.
    #[tracing::instrument(skip(self))]
    pub fn replace_pdu_leaves(&self, room_id: &RoomId, event_ids: &[EventId]) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        for (key, _) in self.roomid_pduleaves.scan_prefix(prefix.clone()) {
            self.roomid_pduleaves.remove(&key)?;
        }

        for event_id in event_ids {
            let mut key = prefix.to_owned();
            key.extend_from_slice(event_id.as_bytes());
            self.roomid_pduleaves.insert(&key, event_id.as_bytes())?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> Result<bool> {
        let mut key = room_id.as_bytes().to_vec();
        key.extend_from_slice(event_id.as_bytes());
        Ok(self.referencedevents.get(&key)?.is_some())
    }

    /// Returns the pdu from the outlier tree.
    #[tracing::instrument(skip(self))]
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
    #[tracing::instrument(skip(self, pdu))]
    pub fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) -> Result<()> {
        self.eventid_outlierpdu.insert(
            &event_id.as_bytes(),
            &serde_json::to_vec(&pdu).expect("CanonicalJsonObject is valid"),
        )
    }

    #[tracing::instrument(skip(self))]
    pub fn mark_event_soft_failed(&self, event_id: &EventId) -> Result<()> {
        self.softfailedeventids.insert(&event_id.as_bytes(), &[])
    }

    #[tracing::instrument(skip(self))]
    pub fn is_event_soft_failed(&self, event_id: &EventId) -> Result<bool> {
        self.softfailedeventids
            .get(&event_id.as_bytes())
            .map(|o| o.is_some())
    }

    /// Creates a new persisted data unit and adds it to a room.
    ///
    /// By this point the incoming event should be fully authenticated, no auth happens
    /// in `append_pdu`.
    ///
    /// Returns pdu id
    #[tracing::instrument(skip(self, pdu, pdu_json, leaves, db))]
    pub fn append_pdu(
        &self,
        pdu: &PduEvent,
        mut pdu_json: CanonicalJsonObject,
        leaves: &[EventId],
        db: &Database,
    ) -> Result<Vec<u8>> {
        let shortroomid = self.get_shortroomid(&pdu.room_id)?.expect("room exists");

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
                                utils::to_canonical_object(prev_state.content.clone())
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
        self.mark_as_referenced(&pdu.room_id, &pdu.prev_events)?;
        self.replace_pdu_leaves(&pdu.room_id, leaves)?;

        let mutex_insert = Arc::clone(
            db.globals
                .roomid_mutex_insert
                .write()
                .unwrap()
                .entry(pdu.room_id.clone())
                .or_default(),
        );
        let insert_lock = mutex_insert.lock().unwrap();

        let count1 = db.globals.next_count()?;
        // Mark as read first so the sending client doesn't get a notification even if appending
        // fails
        self.edus
            .private_read_set(&pdu.room_id, &pdu.sender, count1, &db.globals)?;
        self.reset_notification_counts(&pdu.sender, &pdu.room_id)?;

        let count2 = db.globals.next_count()?;
        let mut pdu_id = shortroomid.to_be_bytes().to_vec();
        pdu_id.extend_from_slice(&count2.to_be_bytes());

        // There's a brief moment of time here where the count is updated but the pdu does not
        // exist. This could theoretically lead to dropped pdus, but it's extremely rare
        //
        // Update: We fixed this using insert_lock

        self.pduid_pdu.insert(
            &pdu_id,
            &serde_json::to_vec(&pdu_json).expect("CanonicalJsonObject is always a valid"),
        )?;

        self.eventid_pduid
            .insert(pdu.event_id.as_bytes(), &pdu_id)?;
        self.eventid_outlierpdu.remove(pdu.event_id.as_bytes())?;

        drop(insert_lock);

        // See if the event matches any known pushers
        let power_levels: PowerLevelsEventContent = db
            .rooms
            .room_state_get(&pdu.room_id, &EventType::RoomPowerLevels, "")?
            .map(|ev| {
                serde_json::from_value(ev.content.clone())
                    .map_err(|_| Error::bad_database("invalid m.room.power_levels event"))
            })
            .transpose()?
            .unwrap_or_default();

        let sync_pdu = pdu.to_sync_room_event();

        let mut notifies = Vec::new();
        let mut highlights = Vec::new();

        for user in self.get_our_real_users(&pdu.room_id, db)?.iter() {
            // Don't notify the user of their own events
            if user == &pdu.sender {
                continue;
            }

            let rules_for_user = db
                .account_data
                .get::<push_rules::PushRulesEvent>(None, &user, EventType::PushRules)?
                .map(|ev| ev.content.global)
                .unwrap_or_else(|| push::Ruleset::server_default(&user));

            let mut highlight = false;
            let mut notify = false;

            for action in pusher::get_actions(
                &user,
                &rules_for_user,
                &power_levels,
                &sync_pdu,
                &pdu.room_id,
                db,
            )? {
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
                notifies.push(userroom_id.clone());
            }

            if highlight {
                highlights.push(userroom_id);
            }

            for senderkey in db.pusher.get_pusher_senderkeys(&user) {
                db.sending.send_push_pdu(&*pdu_id, senderkey)?;
            }
        }

        self.userroomid_notificationcount
            .increment_batch(&mut notifies.into_iter())?;
        self.userroomid_highlightcount
            .increment_batch(&mut highlights.into_iter())?;

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
                        true,
                    )?;
                }
            }
            EventType::RoomMessage => {
                if let Some(body) = pdu.content.get("body").and_then(|b| b.as_str()) {
                    let mut batch = body
                        .split_terminator(|c: char| !c.is_alphanumeric())
                        .filter(|s| !s.is_empty())
                        .filter(|word| word.len() <= 50)
                        .map(str::to_lowercase)
                        .map(|word| {
                            let mut key = shortroomid.to_be_bytes().to_vec();
                            key.extend_from_slice(word.as_bytes());
                            key.push(0xff);
                            key.extend_from_slice(&pdu_id);
                            (key, Vec::new())
                        });

                    self.tokenids.insert_batch(&mut batch)?;

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
                                "get_auth_chain" => {
                                    if args.len() == 1 {
                                        if let Ok(event_id) = EventId::try_from(args[0]) {
                                            if let Some(event) = db.rooms.get_pdu_json(&event_id)? {
                                                let room_id_str = event
                                                    .get("room_id")
                                                    .and_then(|val| val.as_str())
                                                    .ok_or_else(|| {
                                                        Error::bad_database(
                                                            "Invalid event in database",
                                                        )
                                                    })?;

                                                let room_id = RoomId::try_from(room_id_str)
                                                .map_err(|_| Error::bad_database("Invalid room id field in event in database"))?;
                                                let start = Instant::now();
                                                let count = server_server::get_auth_chain(
                                                    &room_id,
                                                    vec![Arc::new(event_id)],
                                                    db,
                                                )?
                                                .count();
                                                let elapsed = start.elapsed();
                                                db.admin.send(AdminCommand::SendMessage(
                                                    message::MessageEventContent::text_plain(
                                                        format!(
                                                    "Loaded auth chain with length {} in {:?}",
                                                    count, elapsed
                                                ),
                                                    ),
                                                ));
                                            }
                                        }
                                    }
                                }
                                "parse_pdu" => {
                                    if body.len() > 2
                                        && body[0].trim() == "```"
                                        && body.last().unwrap().trim() == "```"
                                    {
                                        let string = body[1..body.len() - 1].join("\n");
                                        match serde_json::from_str(&string) {
                                            Ok(value) => {
                                                let event_id = EventId::try_from(&*format!(
                                                    "${}",
                                                    // Anything higher than version3 behaves the same
                                                    ruma::signatures::reference_hash(
                                                        &value,
                                                        &RoomVersionId::Version6
                                                    )
                                                    .expect("ruma can calculate reference hashes")
                                                ))
                                                .expect(
                                                    "ruma's reference hashes are valid event ids",
                                                );

                                                match serde_json::from_value::<PduEvent>(
                                                    serde_json::to_value(value)
                                                        .expect("value is json"),
                                                ) {
                                                    Ok(pdu) => {
                                                        db.admin.send(AdminCommand::SendMessage(
                                                            message::MessageEventContent::text_plain(
                                                                format!("EventId: {:?}\n{:#?}", event_id, pdu),
                                                            ),
                                                        ));
                                                    }
                                                    Err(e) => {
                                                        db.admin.send(AdminCommand::SendMessage(
                                                            message::MessageEventContent::text_plain(
                                                                format!("EventId: {:?}\nCould not parse event: {}", event_id, e),
                                                            ),
                                                        ));
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                db.admin.send(AdminCommand::SendMessage(
                                                    message::MessageEventContent::text_plain(
                                                        format!(
                                                            "Invalid json in command body: {}",
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
                                "get_pdu" => {
                                    if args.len() == 1 {
                                        if let Ok(event_id) = EventId::try_from(args[0]) {
                                            let mut outlier = false;
                                            let mut pdu_json =
                                                db.rooms.get_non_outlier_pdu_json(&event_id)?;
                                            if pdu_json.is_none() {
                                                outlier = true;
                                                pdu_json = db.rooms.get_pdu_json(&event_id)?;
                                            }
                                            match pdu_json {
                                                Some(json) => {
                                                    let json_text =
                                                        serde_json::to_string_pretty(&json)
                                                            .expect("canonical json is valid json");
                                                    db.admin.send(AdminCommand::SendMessage(
                                                        message::MessageEventContent::text_html(
                                                            format!("{}\n```json\n{}\n```",
                                                            if outlier {
                                                                "PDU is outlier"
                                                            } else { "PDU was accepted"}, json_text),
                                                            format!("<p>{}</p>\n<pre><code class=\"language-json\">{}\n</code></pre>\n",
                                                            if outlier {
                                                                "PDU is outlier"
                                                            } else { "PDU was accepted"}, RawStr::new(&json_text).html_escape())
                                                        ),
                                                    ));
                                                }
                                                None => {
                                                    db.admin.send(AdminCommand::SendMessage(
                                                        message::MessageEventContent::text_plain(
                                                            "PDU not found.",
                                                        ),
                                                    ));
                                                }
                                            }
                                        } else {
                                            db.admin.send(AdminCommand::SendMessage(
                                                message::MessageEventContent::text_plain(
                                                    "Event ID could not be parsed.",
                                                ),
                                            ));
                                        }
                                    } else {
                                        db.admin.send(AdminCommand::SendMessage(
                                            message::MessageEventContent::text_plain(
                                                "Usage: get_pdu <eventid>",
                                            ),
                                        ));
                                    }
                                }
                                _ => {
                                    db.admin.send(AdminCommand::SendMessage(
                                        message::MessageEventContent::text_plain(format!(
                                            "Unrecognized command: {}",
                                            command
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

        Ok(pdu_id)
    }

    #[tracing::instrument(skip(self))]
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

    #[tracing::instrument(skip(self))]
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

    #[tracing::instrument(skip(self))]
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
    #[tracing::instrument(skip(self, state_ids_compressed, globals))]
    pub fn set_event_state(
        &self,
        event_id: &EventId,
        room_id: &RoomId,
        state_ids_compressed: HashSet<CompressedStateEvent>,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let shorteventid = self.get_or_create_shorteventid(&event_id, globals)?;

        let previous_shortstatehash = self.current_shortstatehash(&room_id)?;

        let state_hash = self.calculate_hash(
            &state_ids_compressed
                .iter()
                .map(|s| &s[..])
                .collect::<Vec<_>>(),
        );

        let (shortstatehash, already_existed) =
            self.get_or_create_shortstatehash(&state_hash, globals)?;

        if !already_existed {
            let states_parents = previous_shortstatehash
                .map_or_else(|| Ok(Vec::new()), |p| self.load_shortstatehash_info(p))?;

            let (statediffnew, statediffremoved) =
                if let Some(parent_stateinfo) = states_parents.last() {
                    let statediffnew = state_ids_compressed
                        .difference(&parent_stateinfo.1)
                        .cloned()
                        .collect::<HashSet<_>>();

                    let statediffremoved = parent_stateinfo
                        .1
                        .difference(&state_ids_compressed)
                        .cloned()
                        .collect::<HashSet<_>>();

                    (statediffnew, statediffremoved)
                } else {
                    (state_ids_compressed, HashSet::new())
                };
            self.save_state_from_diff(
                shortstatehash,
                statediffnew.clone(),
                statediffremoved.clone(),
                1_000_000, // high number because no state will be based on this one
                states_parents,
            )?;
        }

        self.shorteventid_shortstatehash
            .insert(&shorteventid.to_be_bytes(), &shortstatehash.to_be_bytes())?;

        Ok(())
    }

    /// Generates a new StateHash and associates it with the incoming event.
    ///
    /// This adds all current state events (not including the incoming event)
    /// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
    #[tracing::instrument(skip(self, new_pdu, globals))]
    pub fn append_to_state(
        &self,
        new_pdu: &PduEvent,
        globals: &super::globals::Globals,
    ) -> Result<u64> {
        let shorteventid = self.get_or_create_shorteventid(&new_pdu.event_id, globals)?;

        let previous_shortstatehash = self.current_shortstatehash(&new_pdu.room_id)?;

        if let Some(p) = previous_shortstatehash {
            self.shorteventid_shortstatehash
                .insert(&shorteventid.to_be_bytes(), &p.to_be_bytes())?;
        }

        if let Some(state_key) = &new_pdu.state_key {
            let states_parents = previous_shortstatehash
                .map_or_else(|| Ok(Vec::new()), |p| self.load_shortstatehash_info(p))?;

            let shortstatekey =
                self.get_or_create_shortstatekey(&new_pdu.kind, &state_key, globals)?;

            let new = self.compress_state_event(shortstatekey, &new_pdu.event_id, globals)?;

            let replaces = states_parents
                .last()
                .map(|info| {
                    info.1
                        .iter()
                        .find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
                })
                .unwrap_or_default();

            if Some(&new) == replaces {
                return Ok(previous_shortstatehash.expect("must exist"));
            }

            // TODO: statehash with deterministic inputs
            let shortstatehash = globals.next_count()?;

            let mut statediffnew = HashSet::new();
            statediffnew.insert(new);

            let mut statediffremoved = HashSet::new();
            if let Some(replaces) = replaces {
                statediffremoved.insert(replaces.clone());
            }

            self.save_state_from_diff(
                shortstatehash,
                statediffnew,
                statediffremoved,
                2,
                states_parents,
            )?;

            Ok(shortstatehash)
        } else {
            Ok(previous_shortstatehash.expect("first event in room must be a state event"))
        }
    }

    #[tracing::instrument(skip(self, invite_event))]
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

    #[tracing::instrument(skip(self))]
    pub fn set_room_state(&self, room_id: &RoomId, shortstatehash: u64) -> Result<()> {
        self.roomid_shortstatehash
            .insert(room_id.as_bytes(), &shortstatehash.to_be_bytes())?;

        Ok(())
    }

    pub fn associate_token_shortstatehash(
        &self,
        room_id: &RoomId,
        token: u64,
        shortstatehash: u64,
    ) -> Result<()> {
        let shortroomid = self.get_shortroomid(room_id)?.expect("room exists");

        let mut key = shortroomid.to_be_bytes().to_vec();
        key.extend_from_slice(&token.to_be_bytes());

        self.roomsynctoken_shortstatehash
            .insert(&key, &shortstatehash.to_be_bytes())
    }

    pub fn get_token_shortstatehash(&self, room_id: &RoomId, token: u64) -> Result<Option<u64>> {
        let shortroomid = self.get_shortroomid(room_id)?.expect("room exists");

        let mut key = shortroomid.to_be_bytes().to_vec();
        key.extend_from_slice(&token.to_be_bytes());

        Ok(self
            .roomsynctoken_shortstatehash
            .get(&key)?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid shortstatehash in roomsynctoken_shortstatehash")
                })
            })
            .transpose()?)
    }

    /// Creates a new persisted data unit and adds it to a room.
    #[tracing::instrument(skip(self, db, _mutex_lock))]
    pub fn build_and_append_pdu(
        &self,
        pdu_builder: PduBuilder,
        sender: &UserId,
        room_id: &RoomId,
        db: &Database,
        _mutex_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room mutex
    ) -> Result<EventId> {
        let PduBuilder {
            event_type,
            content,
            unsigned,
            state_key,
            redacts,
        } = pdu_builder;

        let prev_events = self
            .get_pdu_leaves(&room_id)?
            .into_iter()
            .take(20)
            .collect::<Vec<_>>();

        let create_event = self.room_state_get(&room_id, &EventType::RoomCreate, "")?;

        let create_event_content = create_event
            .as_ref()
            .map(|create_event| {
                serde_json::from_value::<Raw<CreateEventContent>>(create_event.content.clone())
                    .expect("Raw::from_value always works.")
                    .deserialize()
                    .map_err(|e| {
                        warn!("Invalid create event: {}", e);
                        Error::bad_database("Invalid create event in db.")
                    })
            })
            .transpose()?;

        let create_prev_event = if prev_events.len() == 1
            && Some(&prev_events[0]) == create_event.as_ref().map(|c| &c.event_id)
        {
            create_event
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
                unsigned.insert("prev_content".to_owned(), prev_pdu.content.clone());
                unsigned.insert(
                    "prev_sender".to_owned(),
                    serde_json::to_value(&prev_pdu.sender).expect("UserId::to_value always works"),
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
            None, // TODO: third_party_invite
            |k, s| auth_events.get(&(k.clone(), s.to_owned())).map(Arc::clone),
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

        // Generate short event id
        let _shorteventid = self.get_or_create_shorteventid(&pdu.event_id, &db.globals)?;

        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        let statehashid = self.append_to_state(&pdu, &db.globals)?;

        let pdu_id = self.append_pdu(
            &pdu,
            pdu_json,
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

        for appservice in db.appservice.all()? {
            if self.appservice_in_room(room_id, &appservice, db)? {
                db.sending.send_pdu_appservice(&appservice.0, &pdu_id)?;
                continue;
            }

            if let Some(namespaces) = appservice.1.get("namespaces") {
                let users = namespaces
                    .get("users")
                    .and_then(|users| users.as_sequence())
                    .map_or_else(Vec::new, |users| {
                        users
                            .iter()
                            .filter_map(|users| Regex::new(users.get("regex")?.as_str()?).ok())
                            .collect::<Vec<_>>()
                    });
                let aliases = namespaces
                    .get("aliases")
                    .and_then(|aliases| aliases.as_sequence())
                    .map_or_else(Vec::new, |aliases| {
                        aliases
                            .iter()
                            .filter_map(|aliases| Regex::new(aliases.get("regex")?.as_str()?).ok())
                            .collect::<Vec<_>>()
                    });
                let rooms = namespaces
                    .get("rooms")
                    .and_then(|rooms| rooms.as_sequence());

                let matching_users = |users: &Regex| {
                    users.is_match(pdu.sender.as_str())
                        || pdu.kind == EventType::RoomMember
                            && pdu
                                .state_key
                                .as_ref()
                                .map_or(false, |state_key| users.is_match(&state_key))
                };
                let matching_aliases = |aliases: &Regex| {
                    self.room_aliases(&room_id)
                        .filter_map(|r| r.ok())
                        .any(|room_alias| aliases.is_match(room_alias.as_str()))
                };

                if aliases.iter().any(matching_aliases)
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
    pub fn all_pdus<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<impl Iterator<Item = Result<(Vec<u8>, PduEvent)>> + 'a> {
        self.pdus_since(user_id, room_id, 0)
    }

    /// Returns an iterator over all events in a room that happened after the event with id `since`
    /// in chronological order.
    #[tracing::instrument(skip(self))]
    pub fn pdus_since<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<impl Iterator<Item = Result<(Vec<u8>, PduEvent)>> + 'a> {
        let prefix = self
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();

        // Skip the first pdu if it's exactly at since, because we sent that last time
        let mut first_pdu_id = prefix.clone();
        first_pdu_id.extend_from_slice(&(since + 1).to_be_bytes());

        let user_id = user_id.clone();

        Ok(self
            .pduid_pdu
            .iter_from(&first_pdu_id, false)
            .take_while(move |(k, _)| k.starts_with(&prefix))
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
    #[tracing::instrument(skip(self))]
    pub fn pdus_until<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        until: u64,
    ) -> Result<impl Iterator<Item = Result<(Vec<u8>, PduEvent)>> + 'a> {
        // Create the first part of the full pdu id
        let prefix = self
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();

        let mut current = prefix.clone();
        current.extend_from_slice(&(until.saturating_sub(1)).to_be_bytes()); // -1 because we don't want event at `until`

        let current: &[u8] = &current;

        let user_id = user_id.clone();

        Ok(self
            .pduid_pdu
            .iter_from(current, true)
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(pdu_id, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((pdu_id, pdu))
            }))
    }

    /// Returns an iterator over all events and their token in a room that happened after the event
    /// with id `from` in chronological order.
    #[tracing::instrument(skip(self))]
    pub fn pdus_after<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        from: u64,
    ) -> Result<impl Iterator<Item = Result<(Vec<u8>, PduEvent)>> + 'a> {
        // Create the first part of the full pdu id
        let prefix = self
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();

        let mut current = prefix.clone();
        current.extend_from_slice(&(from + 1).to_be_bytes()); // +1 so we don't send the base event

        let current: &[u8] = &current;

        let user_id = user_id.clone();

        Ok(self
            .pduid_pdu
            .iter_from(current, false)
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(pdu_id, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((pdu_id, pdu))
            }))
    }

    /// Replace a PDU with the redacted form.
    #[tracing::instrument(skip(self, reason))]
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
    #[tracing::instrument(skip(self, last_state, db))]
    pub fn update_membership(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        membership: member::MembershipState,
        sender: &UserId,
        last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
        db: &Database,
        update_joined_count: bool,
    ) -> Result<()> {
        // Keep track what remote users exist by adding them as "deactivated" users
        if user_id.server_name() != db.globals.server_name() {
            db.users.create(user_id, None)?;
            // TODO: displayname, avatar url
        }

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
                            >(create.content.clone())
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

                if update_joined_count {
                    self.roomserverids.insert(&roomserver_id, &[])?;
                    self.serverroomids.insert(&serverroom_id, &[])?;
                }
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

                if update_joined_count {
                    self.roomserverids.insert(&roomserver_id, &[])?;
                    self.serverroomids.insert(&serverroom_id, &[])?;
                }
                self.userroomid_invitestate.insert(
                    &userroom_id,
                    &serde_json::to_vec(&last_state.unwrap_or_default())
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
                if update_joined_count {
                    if self
                        .room_members(room_id)
                        .chain(self.room_members_invited(room_id))
                        .filter_map(|r| r.ok())
                        .all(|u| u.server_name() != user_id.server_name())
                    {
                        self.roomserverids.remove(&roomserver_id)?;
                        self.serverroomids.remove(&serverroom_id)?;
                    }
                }
                self.userroomid_leftstate.insert(
                    &userroom_id,
                    &serde_json::to_vec(&Vec::<Raw<AnySyncStateEvent>>::new()).unwrap(),
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

        if update_joined_count {
            self.update_joined_count(room_id, db)?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self, room_id, db))]
    pub fn update_joined_count(&self, room_id: &RoomId, db: &Database) -> Result<()> {
        let mut joinedcount = 0_u64;
        let mut invitedcount = 0_u64;
        let mut joined_servers = HashSet::new();
        let mut real_users = HashSet::new();

        for joined in self.room_members(&room_id).filter_map(|r| r.ok()) {
            joined_servers.insert(joined.server_name().to_owned());
            if joined.server_name() == db.globals.server_name()
                && !db.users.is_deactivated(&joined).unwrap_or(true)
            {
                real_users.insert(joined);
            }
            joinedcount += 1;
        }

        for invited in self.room_members_invited(&room_id).filter_map(|r| r.ok()) {
            joined_servers.insert(invited.server_name().to_owned());
            invitedcount += 1;
        }

        self.roomid_joinedcount
            .insert(room_id.as_bytes(), &joinedcount.to_be_bytes())?;

        self.roomid_invitedcount
            .insert(room_id.as_bytes(), &invitedcount.to_be_bytes())?;

        self.our_real_users_cache
            .write()
            .unwrap()
            .insert(room_id.clone(), Arc::new(real_users));

        for old_joined_server in self.room_servers(room_id).filter_map(|r| r.ok()) {
            if !joined_servers.remove(&old_joined_server) {
                // Server not in room anymore
                let mut roomserver_id = room_id.as_bytes().to_vec();
                roomserver_id.push(0xff);
                roomserver_id.extend_from_slice(old_joined_server.as_bytes());

                let mut serverroom_id = old_joined_server.as_bytes().to_vec();
                serverroom_id.push(0xff);
                serverroom_id.extend_from_slice(room_id.as_bytes());

                self.roomserverids.remove(&roomserver_id)?;
                self.serverroomids.remove(&serverroom_id)?;
            }
        }

        // Now only new servers are in joined_servers anymore
        for server in joined_servers {
            let mut roomserver_id = room_id.as_bytes().to_vec();
            roomserver_id.push(0xff);
            roomserver_id.extend_from_slice(server.as_bytes());

            let mut serverroom_id = server.as_bytes().to_vec();
            serverroom_id.push(0xff);
            serverroom_id.extend_from_slice(room_id.as_bytes());

            self.roomserverids.insert(&roomserver_id, &[])?;
            self.serverroomids.insert(&serverroom_id, &[])?;
        }

        self.appservice_in_room_cache
            .write()
            .unwrap()
            .remove(room_id);

        Ok(())
    }

    #[tracing::instrument(skip(self, room_id, db))]
    pub fn get_our_real_users(
        &self,
        room_id: &RoomId,
        db: &Database,
    ) -> Result<Arc<HashSet<UserId>>> {
        let maybe = self
            .our_real_users_cache
            .read()
            .unwrap()
            .get(room_id)
            .cloned();
        if let Some(users) = maybe {
            Ok(users)
        } else {
            self.update_joined_count(room_id, &db)?;
            Ok(Arc::clone(
                self.our_real_users_cache
                    .read()
                    .unwrap()
                    .get(room_id)
                    .unwrap(),
            ))
        }
    }

    #[tracing::instrument(skip(self, room_id, appservice, db))]
    pub fn appservice_in_room(
        &self,
        room_id: &RoomId,
        appservice: &(String, serde_yaml::Value),
        db: &Database,
    ) -> Result<bool> {
        let maybe = self
            .appservice_in_room_cache
            .read()
            .unwrap()
            .get(room_id)
            .and_then(|map| map.get(&appservice.0))
            .copied();

        if let Some(b) = maybe {
            Ok(b)
        } else {
            if let Some(namespaces) = appservice.1.get("namespaces") {
                let users = namespaces
                    .get("users")
                    .and_then(|users| users.as_sequence())
                    .map_or_else(Vec::new, |users| {
                        users
                            .iter()
                            .filter_map(|users| Regex::new(users.get("regex")?.as_str()?).ok())
                            .collect::<Vec<_>>()
                    });

                let bridge_user_id = appservice
                    .1
                    .get("sender_localpart")
                    .and_then(|string| string.as_str())
                    .and_then(|string| {
                        UserId::parse_with_server_name(string, db.globals.server_name()).ok()
                    });

                let in_room = bridge_user_id
                    .map_or(false, |id| self.is_joined(&id, room_id).unwrap_or(false))
                    || self.room_members(&room_id).any(|userid| {
                        userid.map_or(false, |userid| {
                            users.iter().any(|r| r.is_match(userid.as_str()))
                        })
                    });

                self.appservice_in_room_cache
                    .write()
                    .unwrap()
                    .entry(room_id.clone())
                    .or_default()
                    .insert(appservice.0.clone(), in_room);

                Ok(in_room)
            } else {
                Ok(false)
            }
        }
    }

    #[tracing::instrument(skip(self, db))]
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
                true,
            )?;
        } else {
            let mutex_state = Arc::clone(
                db.globals
                    .roomid_mutex_state
                    .write()
                    .unwrap()
                    .entry(room_id.clone())
                    .or_default(),
            );
            let state_lock = mutex_state.lock().await;

            let mut event = serde_json::from_value::<Raw<member::MemberEventContent>>(
                self.room_state_get(room_id, &EventType::RoomMember, &user_id.to_string())?
                    .ok_or(Error::BadRequest(
                        ErrorKind::BadState,
                        "Cannot leave a room you are not a member of.",
                    ))?
                    .content
                    .clone(),
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
                &state_lock,
            )?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self, db))]
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
            Some(version)
                if version == RoomVersionId::Version5 || version == RoomVersionId::Version6 =>
            {
                version
            }
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
    #[tracing::instrument(skip(self))]
    pub fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        self.userroomid_leftstate.remove(&userroom_id)?;
        self.roomuserid_leftcount.remove(&roomuser_id)?;

        Ok(())
    }

    #[tracing::instrument(skip(self, globals))]
    pub fn set_alias(
        &self,
        alias: &RoomAliasId,
        room_id: Option<&RoomId>,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        if let Some(room_id) = room_id {
            // New alias
            self.alias_roomid
                .insert(&alias.alias().as_bytes(), room_id.as_bytes())?;
            let mut aliasid = room_id.as_bytes().to_vec();
            aliasid.push(0xff);
            aliasid.extend_from_slice(&globals.next_count()?.to_be_bytes());
            self.aliasid_alias.insert(&aliasid, &*alias.as_bytes())?;
        } else {
            // room_id=None means remove alias
            if let Some(room_id) = self.alias_roomid.get(&alias.alias().as_bytes())? {
                let mut prefix = room_id.to_vec();
                prefix.push(0xff);

                for (key, _) in self.aliasid_alias.scan_prefix(prefix) {
                    self.aliasid_alias.remove(&key)?;
                }
                self.alias_roomid.remove(&alias.alias().as_bytes())?;
            } else {
                return Err(Error::BadRequest(
                    ErrorKind::NotFound,
                    "Alias does not exist.",
                ));
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn id_from_alias(&self, alias: &RoomAliasId) -> Result<Option<RoomId>> {
        self.alias_roomid
            .get(alias.alias().as_bytes())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(
                    RoomId::try_from(utils::string_from_bytes(&bytes).map_err(|_| {
                        Error::bad_database("Room ID in alias_roomid is invalid unicode.")
                    })?)
                    .map_err(|_| Error::bad_database("Room ID in alias_roomid is invalid."))?,
                ))
            })
    }

    #[tracing::instrument(skip(self))]
    pub fn room_aliases<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<RoomAliasId>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.aliasid_alias.scan_prefix(prefix).map(|(_, bytes)| {
            utils::string_from_bytes(&bytes)
                .map_err(|_| Error::bad_database("Invalid alias bytes in aliasid_alias."))?
                .try_into()
                .map_err(|_| Error::bad_database("Invalid alias in aliasid_alias."))
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn set_public(&self, room_id: &RoomId, public: bool) -> Result<()> {
        if public {
            self.publicroomids.insert(room_id.as_bytes(), &[])?;
        } else {
            self.publicroomids.remove(room_id.as_bytes())?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn is_public_room(&self, room_id: &RoomId) -> Result<bool> {
        Ok(self.publicroomids.get(room_id.as_bytes())?.is_some())
    }

    #[tracing::instrument(skip(self))]
    pub fn public_rooms(&self) -> impl Iterator<Item = Result<RoomId>> + '_ {
        self.publicroomids.iter().map(|(bytes, _)| {
            RoomId::try_from(
                utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Room ID in publicroomids is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Room ID in publicroomids is invalid."))
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<(impl Iterator<Item = Vec<u8>> + 'a, Vec<String>)> {
        let prefix = self
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();
        let prefix_clone = prefix.clone();

        let words = search_string
            .split_terminator(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .collect::<Vec<_>>();

        let iterators = words.clone().into_iter().map(move |word| {
            let mut prefix2 = prefix.clone();
            prefix2.extend_from_slice(word.as_bytes());
            prefix2.push(0xff);

            let mut last_possible_id = prefix2.clone();
            last_possible_id.extend_from_slice(&u64::MAX.to_be_bytes());

            self.tokenids
                .iter_from(&last_possible_id, true) // Newest pdus first
                .take_while(move |(k, _)| k.starts_with(&prefix2))
                .map(|(key, _)| {
                    let pdu_id = key[key.len() - size_of::<u64>()..].to_vec();

                    Ok::<_, Error>(pdu_id)
                })
                .filter_map(|r| r.ok())
        });

        Ok((
            utils::common_elements(iterators, |a, b| {
                // We compare b with a because we reversed the iterator earlier
                b.cmp(a)
            })
            .unwrap()
            .map(move |id| {
                let mut pduid = prefix_clone.clone();
                pduid.extend_from_slice(&id);
                pduid
            }),
            words,
        ))
    }

    #[tracing::instrument(skip(self))]
    pub fn get_shared_rooms<'a>(
        &'a self,
        users: Vec<UserId>,
    ) -> Result<impl Iterator<Item = Result<RoomId>> + 'a> {
        let iterators = users.into_iter().map(move |user_id| {
            let mut prefix = user_id.as_bytes().to_vec();
            prefix.push(0xff);

            self.userroomid_joined
                .scan_prefix(prefix)
                .map(|(key, _)| {
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
        Ok(utils::common_elements(iterators, Ord::cmp)
            .expect("users is not empty")
            .map(|bytes| {
                RoomId::try_from(utils::string_from_bytes(&*bytes).map_err(|_| {
                    Error::bad_database("Invalid RoomId bytes in userroomid_joined")
                })?)
                .map_err(|_| Error::bad_database("Invalid RoomId in userroomid_joined."))
            }))
    }

    /// Returns an iterator of all servers participating in this room.
    #[tracing::instrument(skip(self))]
    pub fn room_servers<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<Box<ServerName>>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomserverids.scan_prefix(prefix).map(|(key, _)| {
            Box::<ServerName>::try_from(
                utils::string_from_bytes(
                    &key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| {
                    Error::bad_database("Server name in roomserverids is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Server name in roomserverids is invalid."))
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn server_in_room<'a>(&'a self, server: &ServerName, room_id: &RoomId) -> Result<bool> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(room_id.as_bytes());

        self.serverroomids.get(&key).map(|o| o.is_some())
    }

    /// Returns an iterator of all rooms a server participates in (as far as we know).
    #[tracing::instrument(skip(self))]
    pub fn server_rooms<'a>(
        &'a self,
        server: &ServerName,
    ) -> impl Iterator<Item = Result<RoomId>> + 'a {
        let mut prefix = server.as_bytes().to_vec();
        prefix.push(0xff);

        self.serverroomids.scan_prefix(prefix).map(|(key, _)| {
            RoomId::try_from(
                utils::string_from_bytes(
                    &key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| Error::bad_database("RoomId in serverroomids is invalid unicode."))?,
            )
            .map_err(|_| Error::bad_database("RoomId in serverroomids is invalid."))
        })
    }

    /// Returns an iterator over all joined members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<UserId>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuserid_joined.scan_prefix(prefix).map(|(key, _)| {
            UserId::try_from(
                utils::string_from_bytes(
                    &key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| {
                    Error::bad_database("User ID in roomuserid_joined is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("User ID in roomuserid_joined is invalid."))
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn room_joined_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
        Ok(self
            .roomid_joinedcount
            .get(room_id.as_bytes())?
            .map(|b| {
                utils::u64_from_bytes(&b)
                    .map_err(|_| Error::bad_database("Invalid joinedcount in db."))
            })
            .transpose()?)
    }

    #[tracing::instrument(skip(self))]
    pub fn room_invited_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
        Ok(self
            .roomid_invitedcount
            .get(room_id.as_bytes())?
            .map(|b| {
                utils::u64_from_bytes(&b)
                    .map_err(|_| Error::bad_database("Invalid joinedcount in db."))
            })
            .transpose()?)
    }

    /// Returns an iterator over all User IDs who ever joined a room.
    #[tracing::instrument(skip(self))]
    pub fn room_useroncejoined<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<UserId>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuseroncejoinedids
            .scan_prefix(prefix)
            .map(|(key, _)| {
                UserId::try_from(
                    utils::string_from_bytes(
                        &key.rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in room_useroncejoined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in room_useroncejoined is invalid."))
            })
    }

    /// Returns an iterator over all invited members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members_invited<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<UserId>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuserid_invitecount
            .scan_prefix(prefix)
            .map(|(key, _)| {
                UserId::try_from(
                    utils::string_from_bytes(
                        &key.rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in roomuserid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in roomuserid_invited is invalid."))
            })
    }

    #[tracing::instrument(skip(self))]
    pub fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_invitecount
            .get(&key)?
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
            .get(&key)?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid leftcount in db.")
                })?))
            })
    }

    /// Returns an iterator over all rooms this user joined.
    #[tracing::instrument(skip(self))]
    pub fn rooms_joined<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<RoomId>> + 'a {
        self.userroomid_joined
            .scan_prefix(user_id.as_bytes().to_vec())
            .map(|(key, _)| {
                RoomId::try_from(
                    utils::string_from_bytes(
                        &key.rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_joined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_joined is invalid."))
            })
    }

    /// Returns an iterator over all rooms a user was invited to.
    #[tracing::instrument(skip(self))]
    pub fn rooms_invited<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(RoomId, Vec<Raw<AnyStrippedStateEvent>>)>> + 'a {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.userroomid_invitestate
            .scan_prefix(prefix)
            .map(|(key, state)| {
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
            .get(&key)?
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
            .get(&key)?
            .map(|state| {
                let state = serde_json::from_slice(&state)
                    .map_err(|_| Error::bad_database("Invalid state in userroomid_leftstate."))?;

                Ok(state)
            })
            .transpose()
    }

    /// Returns an iterator over all rooms a user left.
    #[tracing::instrument(skip(self))]
    pub fn rooms_left<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(RoomId, Vec<Raw<AnySyncStateEvent>>)>> + 'a {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.userroomid_leftstate
            .scan_prefix(prefix)
            .map(|(key, state)| {
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

    #[tracing::instrument(skip(self))]
    pub fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.roomuseroncejoinedids.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    pub fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_joined.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    pub fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_invitestate.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_leftstate.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    pub fn get_auth_chain_from_cache<'a>(
        &'a self,
        key: &[u64],
    ) -> Result<Option<Arc<HashSet<u64>>>> {
        // Check RAM cache
        if let Some(result) = self.auth_chain_cache.lock().unwrap().get_mut(key) {
            return Ok(Some(Arc::clone(result)));
        }

        // Check DB cache
        if key.len() == 1 {
            if let Some(chain) =
                self.shorteventid_authchain
                    .get(&key[0].to_be_bytes())?
                    .map(|chain| {
                        chain
                            .chunks_exact(size_of::<u64>())
                            .map(|chunk| {
                                utils::u64_from_bytes(chunk).expect("byte length is correct")
                            })
                            .collect()
                    })
            {
                let chain = Arc::new(chain);

                // Cache in RAM
                self.auth_chain_cache
                    .lock()
                    .unwrap()
                    .insert(vec![key[0]], Arc::clone(&chain));

                return Ok(Some(chain));
            }
        }

        Ok(None)
    }

    #[tracing::instrument(skip(self))]
    pub fn cache_auth_chain(&self, key: Vec<u64>, chain: Arc<HashSet<u64>>) -> Result<()> {
        // Persist in db
        if key.len() == 1 {
            self.shorteventid_authchain.insert(
                &key[0].to_be_bytes(),
                &chain
                    .iter()
                    .map(|s| s.to_be_bytes().to_vec())
                    .flatten()
                    .collect::<Vec<u8>>(),
            )?;
        }

        // Cache in RAM
        self.auth_chain_cache
            .lock()
            .unwrap()
            .insert(key.clone(), chain);

        Ok(())
    }
}
