mod edus;

pub use edus::RoomEdus;

use crate::{
    pdu::{EventHash, PduBuilder},
    utils, Database, Error, PduEvent, Result,
};
use lru_cache::LruCache;
use regex::Regex;
use ring::digest;
use ruma::{
    api::{client::error::ErrorKind, federation},
    events::{
        direct::DirectEvent,
        ignored_user_list::IgnoredUserListEvent,
        push_rules::PushRulesEvent,
        room::{
            create::RoomCreateEventContent,
            member::{MembershipState, RoomMemberEventContent},
            power_levels::RoomPowerLevelsEventContent,
        },
        tag::TagEvent,
        AnyStrippedStateEvent, AnySyncStateEvent, GlobalAccountDataEventType,
        RoomAccountDataEventType, RoomEventType, StateEventType,
    },
    push::{Action, Ruleset, Tweak},
    serde::{CanonicalJsonObject, CanonicalJsonValue, Raw},
    state_res::{self, RoomVersion, StateMap},
    uint, DeviceId, EventId, RoomAliasId, RoomId, RoomVersionId, ServerName, UserId,
};
use serde::Deserialize;
use serde_json::value::to_raw_value;
use std::{
    borrow::Cow,
    collections::{hash_map, BTreeMap, HashMap, HashSet},
    fmt::Debug,
    iter,
    mem::size_of,
    sync::{Arc, Mutex, RwLock},
};
use tokio::sync::MutexGuard;
use tracing::{error, warn};

use super::{abstraction::Tree, pusher};

/// The unique identifier of each state group.
///
/// This is created when a state group is added to the database by
/// hashing the entire state.
pub type StateHashId = Vec<u8>;
pub type CompressedStateEvent = [u8; 2 * size_of::<u64>()];

pub struct Rooms {
    pub edus: RoomEdus,
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

    pub(super) disabledroomids: Arc<dyn Tree>, // Rooms where incoming federation handling is disabled

    pub(super) lazyloadedids: Arc<dyn Tree>, // LazyLoadedIds = UserId + DeviceId + RoomId + LazyLoadedUserId

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

    pub(super) pdu_cache: Mutex<LruCache<Box<EventId>, Arc<PduEvent>>>,
    pub(super) shorteventid_cache: Mutex<LruCache<u64, Arc<EventId>>>,
    pub(super) auth_chain_cache: Mutex<LruCache<Vec<u64>, Arc<HashSet<u64>>>>,
    pub(super) eventidshort_cache: Mutex<LruCache<Box<EventId>, u64>>,
    pub(super) statekeyshort_cache: Mutex<LruCache<(StateEventType, String), u64>>,
    pub(super) shortstatekey_cache: Mutex<LruCache<u64, (StateEventType, String)>>,
    pub(super) our_real_users_cache: RwLock<HashMap<Box<RoomId>, Arc<HashSet<Box<UserId>>>>>,
    pub(super) appservice_in_room_cache: RwLock<HashMap<Box<RoomId>, HashMap<String, bool>>>,
    pub(super) lazy_load_waiting:
        Mutex<HashMap<(Box<UserId>, Box<DeviceId>, Box<RoomId>, u64), HashSet<Box<UserId>>>>,
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
    pub(super) lasttimelinecount_cache: Mutex<HashMap<Box<RoomId>, u64>>,
}

impl Rooms {
    /// Returns true if a given room version is supported
    #[tracing::instrument(skip(self, db))]
    pub fn is_supported_version(&self, db: &Database, room_version: &RoomVersionId) -> bool {
        db.globals.supported_room_versions().contains(room_version)
    }

    /// This fetches auth events from the current state.
    #[tracing::instrument(skip(self))]
    pub fn get_auth_events(
        &self,
        room_id: &RoomId,
        kind: &RoomEventType,
        sender: &UserId,
        state_key: Option<&str>,
        content: &serde_json::value::RawValue,
    ) -> Result<StateMap<Arc<PduEvent>>> {
        let shortstatehash =
            if let Some(current_shortstatehash) = self.current_shortstatehash(room_id)? {
                current_shortstatehash
            } else {
                return Ok(HashMap::new());
            };

        let auth_events = state_res::auth_types_for_event(kind, sender, state_key, content)
            .expect("content is a valid JSON object");

        let mut sauthevents = auth_events
            .into_iter()
            .filter_map(|(event_type, state_key)| {
                self.get_shortstatekey(&event_type.to_string().into(), &state_key)
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

    #[tracing::instrument(skip(self))]
    pub fn iter_ids(&self) -> impl Iterator<Item = Result<Box<RoomId>>> + '_ {
        self.roomid_shortroomid.iter().map(|(bytes, _)| {
            RoomId::parse(
                utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Room ID in publicroomids is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Room ID in roomid_shortroomid is invalid."))
        })
    }

    pub fn is_disabled(&self, room_id: &RoomId) -> Result<bool> {
        Ok(self.disabledroomids.get(room_id.as_bytes())?.is_some())
    }

}
