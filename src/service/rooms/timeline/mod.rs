mod data;

use std::{
    cmp::Ordering,
    collections::{BTreeMap, HashMap, HashSet},
    sync::Arc,
};

pub use data::Data;

use ruma::{
    api::{client::error::ErrorKind, federation},
    canonical_json::to_canonical_value,
    events::{
        push_rules::PushRulesEvent,
        room::{
            canonical_alias::RoomCanonicalAliasEventContent, create::RoomCreateEventContent,
            encrypted::Relation, member::MembershipState,
            power_levels::RoomPowerLevelsEventContent, redaction::RoomRedactionEventContent,
        },
        GlobalAccountDataEventType, StateEventType, TimelineEventType,
    },
    push::{Action, Ruleset, Tweak},
    state_res::{self, Event, RoomVersion},
    uint, user_id, CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch,
    OwnedEventId, OwnedRoomId, OwnedServerName, RoomId, RoomVersionId, ServerName, UserId,
};
use serde::Deserialize;
use serde_json::value::{to_raw_value, RawValue as RawJsonValue};
use tokio::sync::{Mutex, MutexGuard, RwLock};
use tracing::{error, info, warn};

use crate::{
    api::server_server,
    service::{
        globals::SigningKeys,
        pdu::{EventHash, PduBuilder},
    },
    services, utils, Error, PduEvent, Result,
};

use super::state_compressor::CompressedStateEvent;

#[derive(Hash, PartialEq, Eq, Clone, Copy, Debug)]
pub enum PduCount {
    Backfilled(u64),
    Normal(u64),
}

impl PduCount {
    pub fn min() -> Self {
        Self::Backfilled(u64::MAX)
    }
    pub fn max() -> Self {
        Self::Normal(u64::MAX)
    }

    pub fn try_from_string(token: &str) -> Result<Self> {
        if let Some(stripped) = token.strip_prefix('-') {
            stripped.parse().map(PduCount::Backfilled)
        } else {
            token.parse().map(PduCount::Normal)
        }
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid pagination token."))
    }

    pub fn stringify(&self) -> String {
        match self {
            PduCount::Backfilled(x) => format!("-{x}"),
            PduCount::Normal(x) => x.to_string(),
        }
    }
}

impl PartialOrd for PduCount {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PduCount {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (PduCount::Normal(s), PduCount::Normal(o)) => s.cmp(o),
            (PduCount::Backfilled(s), PduCount::Backfilled(o)) => o.cmp(s),
            (PduCount::Normal(_), PduCount::Backfilled(_)) => Ordering::Greater,
            (PduCount::Backfilled(_), PduCount::Normal(_)) => Ordering::Less,
        }
    }
}

pub struct Service {
    pub db: &'static dyn Data,

    pub lasttimelinecount_cache: Mutex<HashMap<OwnedRoomId, PduCount>>,
}

impl Service {
    #[tracing::instrument(skip(self))]
    pub fn first_pdu_in_room(&self, room_id: &RoomId) -> Result<Option<Arc<PduEvent>>> {
        self.all_pdus(user_id!("@doesntmatter:conduit.rs"), room_id)?
            .next()
            .map(|o| o.map(|(_, p)| Arc::new(p)))
            .transpose()
    }

    #[tracing::instrument(skip(self))]
    pub fn last_timeline_count(&self, sender_user: &UserId, room_id: &RoomId) -> Result<PduCount> {
        self.db.last_timeline_count(sender_user, room_id)
    }

    /// Returns the `count` of this pdu's id.
    pub fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<PduCount>> {
        self.db.get_pdu_count(event_id)
    }

    // TODO Is this the same as the function above?
    /*
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
    */

    /// Returns the json of a pdu.
    pub fn get_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>> {
        self.db.get_pdu_json(event_id)
    }

    /// Returns the json of a pdu.
    pub fn get_non_outlier_pdu_json(
        &self,
        event_id: &EventId,
    ) -> Result<Option<CanonicalJsonObject>> {
        self.db.get_non_outlier_pdu_json(event_id)
    }

    /// Returns the pdu's id.
    pub fn get_pdu_id(&self, event_id: &EventId) -> Result<Option<Vec<u8>>> {
        self.db.get_pdu_id(event_id)
    }

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    pub fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.db.get_non_outlier_pdu(event_id)
    }

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    pub fn get_pdu(&self, event_id: &EventId) -> Result<Option<Arc<PduEvent>>> {
        self.db.get_pdu(event_id)
    }

    /// Returns the pdu.
    ///
    /// This does __NOT__ check the outliers `Tree`.
    pub fn get_pdu_from_id(&self, pdu_id: &[u8]) -> Result<Option<PduEvent>> {
        self.db.get_pdu_from_id(pdu_id)
    }

    /// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
    pub fn get_pdu_json_from_id(&self, pdu_id: &[u8]) -> Result<Option<CanonicalJsonObject>> {
        self.db.get_pdu_json_from_id(pdu_id)
    }

    /// Removes a pdu and creates a new one with the same id.
    #[tracing::instrument(skip(self))]
    pub fn replace_pdu(
        &self,
        pdu_id: &[u8],
        pdu_json: &CanonicalJsonObject,
        pdu: &PduEvent,
    ) -> Result<()> {
        self.db.replace_pdu(pdu_id, pdu_json, pdu)
    }

    /// Creates a new persisted data unit and adds it to a room.
    ///
    /// By this point the incoming event should be fully authenticated, no auth happens
    /// in `append_pdu`.
    ///
    /// Returns pdu id
    #[tracing::instrument(skip(self, pdu, pdu_json, leaves))]
    pub async fn append_pdu<'a>(
        &self,
        pdu: &PduEvent,
        mut pdu_json: CanonicalJsonObject,
        leaves: Vec<OwnedEventId>,
        state_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<Vec<u8>> {
        let shortroomid = services()
            .rooms
            .short
            .get_shortroomid(&pdu.room_id)?
            .expect("room exists");

        // Make unsigned fields correct. This is not properly documented in the spec, but state
        // events need to have previous content in the unsigned field, so clients can easily
        // interpret things like membership changes
        if let Some(state_key) = &pdu.state_key {
            if let CanonicalJsonValue::Object(unsigned) = pdu_json
                .entry("unsigned".to_owned())
                .or_insert_with(|| CanonicalJsonValue::Object(Default::default()))
            {
                if let Some(shortstatehash) = services()
                    .rooms
                    .state_accessor
                    .pdu_shortstatehash(&pdu.event_id)
                    .unwrap()
                {
                    if let Some(prev_state) = services()
                        .rooms
                        .state_accessor
                        .state_get(shortstatehash, &pdu.kind.to_string().into(), state_key)
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
        services()
            .rooms
            .pdu_metadata
            .mark_as_referenced(&pdu.room_id, &pdu.prev_events)?;
        services()
            .rooms
            .state
            .set_forward_extremities(&pdu.room_id, leaves, state_lock)?;

        let mutex_insert = Arc::clone(
            services()
                .globals
                .roomid_mutex_insert
                .write()
                .await
                .entry(pdu.room_id.clone())
                .or_default(),
        );
        let insert_lock = mutex_insert.lock().await;

        let count1 = services().globals.next_count()?;
        // Mark as read first so the sending client doesn't get a notification even if appending
        // fails
        services()
            .rooms
            .edus
            .read_receipt
            .private_read_set(&pdu.room_id, &pdu.sender, count1)?;
        services()
            .rooms
            .user
            .reset_notification_counts(&pdu.sender, &pdu.room_id)?;

        let count2 = services().globals.next_count()?;
        let mut pdu_id = shortroomid.to_be_bytes().to_vec();
        pdu_id.extend_from_slice(&count2.to_be_bytes());

        // Insert pdu
        self.db.append_pdu(&pdu_id, pdu, &pdu_json, count2)?;

        drop(insert_lock);

        // See if the event matches any known pushers
        let power_levels: RoomPowerLevelsEventContent = services()
            .rooms
            .state_accessor
            .room_state_get(&pdu.room_id, &StateEventType::RoomPowerLevels, "")?
            .map(|ev| {
                serde_json::from_str(ev.content.get())
                    .map_err(|_| Error::bad_database("invalid m.room.power_levels event"))
            })
            .transpose()?
            .unwrap_or_default();

        let sync_pdu = pdu.to_sync_room_event();

        let mut notifies = Vec::new();
        let mut highlights = Vec::new();

        let mut push_target = services()
            .rooms
            .state_cache
            .get_our_real_users(&pdu.room_id)?;

        if pdu.kind == TimelineEventType::RoomMember {
            if let Some(state_key) = &pdu.state_key {
                let target_user_id = UserId::parse(state_key.clone())
                    .expect("This state_key was previously validated");

                if !push_target.contains(&target_user_id) {
                    let mut target = push_target.as_ref().clone();
                    target.insert(target_user_id);
                    push_target = Arc::new(target);
                }
            }
        }

        for user in push_target.iter() {
            // Don't notify the user of their own events
            if user == &pdu.sender {
                continue;
            }

            let rules_for_user = services()
                .account_data
                .get(
                    None,
                    user,
                    GlobalAccountDataEventType::PushRules.to_string().into(),
                )?
                .map(|event| {
                    serde_json::from_str::<PushRulesEvent>(event.get())
                        .map_err(|_| Error::bad_database("Invalid push rules event in db."))
                })
                .transpose()?
                .map(|ev: PushRulesEvent| ev.content.global)
                .unwrap_or_else(|| Ruleset::server_default(user));

            let mut highlight = false;
            let mut notify = false;

            for action in services().pusher.get_actions(
                user,
                &rules_for_user,
                &power_levels,
                &sync_pdu,
                &pdu.room_id,
            )? {
                match action {
                    Action::Notify => notify = true,
                    Action::SetTweak(Tweak::Highlight(true)) => {
                        highlight = true;
                    }
                    _ => {}
                };
            }

            if notify {
                notifies.push(user.clone());
            }

            if highlight {
                highlights.push(user.clone());
            }

            for push_key in services().pusher.get_pushkeys(user) {
                services().sending.send_push_pdu(&pdu_id, user, push_key?)?;
            }
        }

        self.db
            .increment_notification_counts(&pdu.room_id, notifies, highlights)?;

        match pdu.kind {
            TimelineEventType::RoomRedaction => {
                let room_version_id = services().rooms.state.get_room_version(&pdu.room_id)?;
                match room_version_id {
                    RoomVersionId::V1
                    | RoomVersionId::V2
                    | RoomVersionId::V3
                    | RoomVersionId::V4
                    | RoomVersionId::V5
                    | RoomVersionId::V6
                    | RoomVersionId::V7
                    | RoomVersionId::V8
                    | RoomVersionId::V9
                    | RoomVersionId::V10 => {
                        if let Some(redact_id) = &pdu.redacts {
                            if services().rooms.state_accessor.user_can_redact(
                                redact_id,
                                &pdu.sender,
                                &pdu.room_id,
                                false,
                            )? {
                                self.redact_pdu(redact_id, pdu, shortroomid)?;
                            }
                        }
                    }
                    RoomVersionId::V11 => {
                        let content =
                            serde_json::from_str::<RoomRedactionEventContent>(pdu.content.get())
                                .map_err(|_| {
                                    Error::bad_database("Invalid content in redaction pdu.")
                                })?;
                        if let Some(redact_id) = &content.redacts {
                            if services().rooms.state_accessor.user_can_redact(
                                redact_id,
                                &pdu.sender,
                                &pdu.room_id,
                                false,
                            )? {
                                self.redact_pdu(redact_id, pdu, shortroomid)?;
                            }
                        }
                    }
                    _ => unreachable!("Validity of room version already checked"),
                };
            }
            TimelineEventType::SpaceChild => {
                if let Some(_state_key) = &pdu.state_key {
                    services()
                        .rooms
                        .spaces
                        .roomid_spacehierarchy_cache
                        .lock()
                        .await
                        .remove(&pdu.room_id);
                }
            }
            TimelineEventType::RoomMember => {
                if let Some(state_key) = &pdu.state_key {
                    #[derive(Deserialize)]
                    struct ExtractMembership {
                        membership: MembershipState,
                    }

                    // if the state_key fails
                    let target_user_id = UserId::parse(state_key.clone())
                        .expect("This state_key was previously validated");

                    let content = serde_json::from_str::<ExtractMembership>(pdu.content.get())
                        .map_err(|_| Error::bad_database("Invalid content in pdu."))?;

                    let stripped_state = match content.membership {
                        MembershipState::Invite | MembershipState::Knock => {
                            let mut state = services().rooms.state.stripped_state(&pdu.room_id)?;
                            // So that clients can get info about who invitied them (not relevant for knocking), the reason, when, etc.
                            state.push(pdu.to_stripped_state_event());
                            Some(state)
                        }
                        _ => None,
                    };

                    // Here we don't attempt to join if the previous membership was knock and the
                    // new one is join, like we do for `/federation/*/invite`, as not only are there
                    // implementation difficulties due to callers not implementing `Send`, but
                    // invites we recieve which aren't over `/invite` must have been due to a
                    // database reset or switching server implementations, which means we probably
                    // shouldn't be joining automatically anyways, since it may surprise users to
                    // suddenly join rooms which clients didn't even show as being knocked on before.
                    services().rooms.state_cache.update_membership(
                        &pdu.room_id,
                        &target_user_id,
                        content.membership,
                        &pdu.sender,
                        stripped_state,
                        true,
                    )?;
                }
            }
            TimelineEventType::RoomMessage => {
                #[derive(Deserialize)]
                struct ExtractBody {
                    body: Option<String>,
                }

                let content = serde_json::from_str::<ExtractBody>(pdu.content.get())
                    .map_err(|_| Error::bad_database("Invalid content in pdu."))?;

                if let Some(body) = content.body {
                    services()
                        .rooms
                        .search
                        .index_pdu(shortroomid, &pdu_id, &body)?;

                    let server_user = services().globals.server_user();

                    let to_conduit = body.starts_with(&format!("{server_user}: "))
                        || body.starts_with(&format!("{server_user} "))
                        || body == format!("{server_user}:")
                        || body == server_user.as_str();

                    // This will evaluate to false if the emergency password is set up so that
                    // the administrator can execute commands as conduit
                    let from_conduit = pdu.sender == *server_user
                        && services().globals.emergency_password().is_none();

                    if let Some(admin_room) = services().admin.get_admin_room()? {
                        if to_conduit
                            && !from_conduit
                            && admin_room == pdu.room_id
                            && services()
                                .rooms
                                .state_cache
                                .is_joined(server_user, &admin_room)?
                        {
                            services().admin.process_message(body);
                        }
                    }
                }
            }
            _ => {}
        }

        // Update Relationships
        #[derive(Deserialize)]
        struct ExtractRelatesTo {
            #[serde(rename = "m.relates_to")]
            relates_to: Relation,
        }

        #[derive(Clone, Debug, Deserialize)]
        struct ExtractEventId {
            event_id: OwnedEventId,
        }
        #[derive(Clone, Debug, Deserialize)]
        struct ExtractRelatesToEventId {
            #[serde(rename = "m.relates_to")]
            relates_to: ExtractEventId,
        }

        if let Ok(content) = serde_json::from_str::<ExtractRelatesToEventId>(pdu.content.get()) {
            if let Some(related_pducount) = services()
                .rooms
                .timeline
                .get_pdu_count(&content.relates_to.event_id)?
            {
                services()
                    .rooms
                    .pdu_metadata
                    .add_relation(PduCount::Normal(count2), related_pducount)?;
            }
        }

        if let Ok(content) = serde_json::from_str::<ExtractRelatesTo>(pdu.content.get()) {
            match content.relates_to {
                Relation::Reply { in_reply_to } => {
                    // We need to do it again here, because replies don't have
                    // event_id as a top level field
                    if let Some(related_pducount) = services()
                        .rooms
                        .timeline
                        .get_pdu_count(&in_reply_to.event_id)?
                    {
                        services()
                            .rooms
                            .pdu_metadata
                            .add_relation(PduCount::Normal(count2), related_pducount)?;
                    }
                }
                Relation::Thread(thread) => {
                    services()
                        .rooms
                        .threads
                        .add_to_thread(&thread.event_id, pdu)?;
                }
                _ => {} // TODO: Aggregate other types
            }
        }

        for appservice in services().appservice.read().await.values() {
            if services()
                .rooms
                .state_cache
                .appservice_in_room(&pdu.room_id, appservice)?
            {
                services()
                    .sending
                    .send_pdu_appservice(appservice.registration.id.clone(), pdu_id.clone())?;
                continue;
            }

            // If the RoomMember event has a non-empty state_key, it is targeted at someone.
            // If it is our appservice user, we send this PDU to it.
            if pdu.kind == TimelineEventType::RoomMember {
                if let Some(state_key_uid) = &pdu
                    .state_key
                    .as_ref()
                    .and_then(|state_key| UserId::parse(state_key.as_str()).ok())
                {
                    let appservice_uid = appservice.registration.sender_localpart.as_str();
                    if state_key_uid == appservice_uid {
                        services().sending.send_pdu_appservice(
                            appservice.registration.id.clone(),
                            pdu_id.clone(),
                        )?;
                        continue;
                    }
                }
            }

            let matching_users = || {
                services().globals.server_name() == pdu.sender.server_name()
                    && appservice.is_user_match(&pdu.sender)
                    || pdu.kind == TimelineEventType::RoomMember
                        && pdu.state_key.as_ref().map_or(false, |state_key| {
                            UserId::parse(state_key).map_or(false, |user_id| {
                                services().globals.server_name() == user_id.server_name()
                                    && appservice.is_user_match(&user_id)
                            })
                        })
            };

            let matching_aliases = || {
                services()
                    .rooms
                    .alias
                    .local_aliases_for_room(&pdu.room_id)
                    .filter_map(Result::ok)
                    .any(|room_alias| appservice.aliases.is_match(room_alias.as_str()))
                    || if let Ok(Some(pdu)) = services().rooms.state_accessor.room_state_get(
                        &pdu.room_id,
                        &StateEventType::RoomCanonicalAlias,
                        "",
                    ) {
                        serde_json::from_str::<RoomCanonicalAliasEventContent>(pdu.content.get())
                            .map_or(false, |content| {
                                content.alias.map_or(false, |alias| {
                                    appservice.aliases.is_match(alias.as_str())
                                }) || content
                                    .alt_aliases
                                    .iter()
                                    .any(|alias| appservice.aliases.is_match(alias.as_str()))
                            })
                    } else {
                        false
                    }
            };

            if matching_aliases()
                || appservice.rooms.is_match(pdu.room_id.as_str())
                || matching_users()
            {
                services()
                    .sending
                    .send_pdu_appservice(appservice.registration.id.clone(), pdu_id.clone())?;
            }
        }

        Ok(pdu_id)
    }

    pub fn create_hash_and_sign_event(
        &self,
        pdu_builder: PduBuilder,
        sender: &UserId,
        room_id: &RoomId,
        _mutex_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<(PduEvent, CanonicalJsonObject)> {
        let PduBuilder {
            event_type,
            content,
            unsigned,
            state_key,
            redacts,
            timestamp,
        } = pdu_builder;

        let prev_events: Vec<_> = services()
            .rooms
            .state
            .get_forward_extremities(room_id)?
            .into_iter()
            .take(20)
            .collect();

        // If there was no create event yet, assume we are creating a room
        let room_version_id = services()
            .rooms
            .state
            .get_room_version(room_id)
            .or_else(|_| {
                if event_type == TimelineEventType::RoomCreate {
                    let content = serde_json::from_str::<RoomCreateEventContent>(content.get())
                        .expect("Invalid content in RoomCreate pdu.");
                    Ok(content.room_version)
                } else {
                    Err(Error::InconsistentRoomState(
                        "non-create event for room of unknown version",
                        room_id.to_owned(),
                    ))
                }
            })?;

        let room_version = RoomVersion::new(&room_version_id).expect("room version is supported");

        let auth_events = services().rooms.state.get_auth_events(
            room_id,
            &event_type,
            sender,
            state_key.as_deref(),
            &content,
        )?;

        // Our depth is the maximum depth of prev_events + 1
        let depth = prev_events
            .iter()
            .filter_map(|event_id| Some(services().rooms.timeline.get_pdu(event_id).ok()??.depth))
            .max()
            .unwrap_or_else(|| uint!(0))
            + uint!(1);

        let mut unsigned = unsigned.unwrap_or_default();

        if let Some(state_key) = &state_key {
            if let Some(prev_pdu) = services().rooms.state_accessor.room_state_get(
                room_id,
                &event_type.to_string().into(),
                state_key,
            )? {
                unsigned.insert(
                    "prev_content".to_owned(),
                    serde_json::from_str(prev_pdu.content.get()).expect("string is valid json"),
                );
                unsigned.insert(
                    "prev_sender".to_owned(),
                    serde_json::to_value(&prev_pdu.sender).expect("UserId::to_value always works"),
                );
            }
        }

        let mut pdu = PduEvent {
            event_id: ruma::event_id!("$thiswillbefilledinlater").into(),
            room_id: room_id.to_owned(),
            sender: sender.to_owned(),
            origin_server_ts: timestamp
                .map(|ts| ts.get())
                .unwrap_or_else(|| MilliSecondsSinceUnixEpoch::now().get()),
            kind: event_type,
            content,
            state_key,
            prev_events,
            depth,
            auth_events: auth_events
                .values()
                .map(|pdu| pdu.event_id.clone())
                .collect(),
            redacts,
            unsigned: if unsigned.is_empty() {
                None
            } else {
                Some(to_raw_value(&unsigned).expect("to_raw_value always works"))
            },
            hashes: EventHash {
                sha256: "aaa".to_owned(),
            },
            signatures: None,
        };

        let auth_check = state_res::auth_check(&room_version, &pdu, |k, s| {
            auth_events.get(&(k.clone(), s.to_owned()))
        })
        .map_err(|e| {
            error!("{:?}", e);
            Error::bad_database("Auth check failed.")
        })?;

        if !auth_check {
            return Err(Error::BadRequest(
                ErrorKind::forbidden(),
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
            to_canonical_value(services().globals.server_name())
                .expect("server name is a valid CanonicalJsonValue"),
        );

        match ruma::signatures::hash_and_sign_event(
            services().globals.server_name().as_str(),
            services().globals.keypair(),
            &mut pdu_json,
            &room_version_id,
        ) {
            Ok(_) => {}
            Err(e) => {
                return match e {
                    ruma::signatures::Error::PduSize => Err(Error::BadRequest(
                        ErrorKind::TooLarge,
                        "Message is too long",
                    )),
                    _ => Err(Error::BadRequest(
                        ErrorKind::Unknown,
                        "Signing event failed",
                    )),
                }
            }
        }

        // Generate event id
        pdu.event_id = EventId::parse_arc(format!(
            "${}",
            ruma::signatures::reference_hash(&pdu_json, &room_version_id)
                .expect("Event format validated when event was hashed")
        ))
        .expect("ruma's reference hashes are valid event ids");

        pdu_json.insert(
            "event_id".to_owned(),
            CanonicalJsonValue::String(pdu.event_id.as_str().to_owned()),
        );

        // Generate short event id
        let _shorteventid = services()
            .rooms
            .short
            .get_or_create_shorteventid(&pdu.event_id)?;

        Ok((pdu, pdu_json))
    }

    /// Creates a new persisted data unit and adds it to a room. This function takes a
    /// roomid_mutex_state, meaning that only this function is able to mutate the room state.
    #[tracing::instrument(skip(self, state_lock))]
    pub async fn build_and_append_pdu(
        &self,
        pdu_builder: PduBuilder,
        sender: &UserId,
        room_id: &RoomId,
        state_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<Arc<EventId>> {
        let (pdu, pdu_json) =
            self.create_hash_and_sign_event(pdu_builder, sender, room_id, state_lock)?;

        if let Some(admin_room) = services().admin.get_admin_room()? {
            if admin_room == room_id {
                match pdu.event_type() {
                    TimelineEventType::RoomEncryption => {
                        warn!("Encryption is not allowed in the admins room");
                        return Err(Error::BadRequest(
                            ErrorKind::forbidden(),
                            "Encryption is not allowed in the admins room.",
                        ));
                    }
                    TimelineEventType::RoomMember => {
                        #[derive(Deserialize)]
                        struct ExtractMembership {
                            membership: MembershipState,
                        }

                        let target = pdu
                            .state_key()
                            .filter(|v| v.starts_with('@'))
                            .unwrap_or(sender.as_str());
                        let server_name = services().globals.server_name();
                        let server_user = services().globals.server_user().as_str();
                        let content = serde_json::from_str::<ExtractMembership>(pdu.content.get())
                            .map_err(|_| Error::bad_database("Invalid content in pdu."))?;

                        if content.membership == MembershipState::Leave {
                            if target == server_user {
                                warn!("Conduit user cannot leave from admins room");
                                return Err(Error::BadRequest(
                                    ErrorKind::forbidden(),
                                    "Conduit user cannot leave from admins room.",
                                ));
                            }

                            let count = services()
                                .rooms
                                .state_cache
                                .room_members(room_id)
                                .filter_map(|m| m.ok())
                                .filter(|m| m.server_name() == server_name)
                                .filter(|m| m != target)
                                .count();
                            if count < 2 {
                                warn!("Last admin cannot leave from admins room");
                                return Err(Error::BadRequest(
                                    ErrorKind::forbidden(),
                                    "Last admin cannot leave from admins room.",
                                ));
                            }
                        }

                        if content.membership == MembershipState::Ban && pdu.state_key().is_some() {
                            if target == server_user {
                                warn!("Conduit user cannot be banned in admins room");
                                return Err(Error::BadRequest(
                                    ErrorKind::forbidden(),
                                    "Conduit user cannot be banned in admins room.",
                                ));
                            }

                            let count = services()
                                .rooms
                                .state_cache
                                .room_members(room_id)
                                .filter_map(|m| m.ok())
                                .filter(|m| m.server_name() == server_name)
                                .filter(|m| m != target)
                                .count();
                            if count < 2 {
                                warn!("Last admin cannot be banned in admins room");
                                return Err(Error::BadRequest(
                                    ErrorKind::forbidden(),
                                    "Last admin cannot be banned in admins room.",
                                ));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        // If redaction event is not authorized, do not append it to the timeline
        if pdu.kind == TimelineEventType::RoomRedaction {
            match services().rooms.state.get_room_version(&pdu.room_id)? {
                RoomVersionId::V1
                | RoomVersionId::V2
                | RoomVersionId::V3
                | RoomVersionId::V4
                | RoomVersionId::V5
                | RoomVersionId::V6
                | RoomVersionId::V7
                | RoomVersionId::V8
                | RoomVersionId::V9
                | RoomVersionId::V10 => {
                    if let Some(redact_id) = &pdu.redacts {
                        if !services().rooms.state_accessor.user_can_redact(
                            redact_id,
                            &pdu.sender,
                            &pdu.room_id,
                            false,
                        )? {
                            return Err(Error::BadRequest(
                                ErrorKind::forbidden(),
                                "User cannot redact this event.",
                            ));
                        }
                    };
                }
                RoomVersionId::V11 => {
                    let content =
                        serde_json::from_str::<RoomRedactionEventContent>(pdu.content.get())
                            .map_err(|_| {
                                Error::bad_database("Invalid content in redaction pdu.")
                            })?;

                    if let Some(redact_id) = &content.redacts {
                        if !services().rooms.state_accessor.user_can_redact(
                            redact_id,
                            &pdu.sender,
                            &pdu.room_id,
                            false,
                        )? {
                            return Err(Error::BadRequest(
                                ErrorKind::forbidden(),
                                "User cannot redact this event.",
                            ));
                        }
                    }
                }
                _ => {
                    return Err(Error::BadRequest(
                        ErrorKind::UnsupportedRoomVersion,
                        "Unsupported room version",
                    ));
                }
            }
        }

        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        let statehashid = services().rooms.state.append_to_state(&pdu)?;

        let pdu_id = self
            .append_pdu(
                &pdu,
                pdu_json,
                // Since this PDU references all pdu_leaves we can update the leaves
                // of the room
                vec![(*pdu.event_id).to_owned()],
                state_lock,
            )
            .await?;

        // We set the room state after inserting the pdu, so that we never have a moment in time
        // where events in the current room state do not exist
        services()
            .rooms
            .state
            .set_room_state(room_id, statehashid, state_lock)?;

        let mut servers: HashSet<OwnedServerName> = services()
            .rooms
            .state_cache
            .room_servers(room_id)
            .filter_map(|r| r.ok())
            .collect();

        // In case we are kicking or banning a user, we need to inform their server of the change
        if pdu.kind == TimelineEventType::RoomMember {
            if let Some(state_key_uid) = &pdu
                .state_key
                .as_ref()
                .and_then(|state_key| UserId::parse(state_key.as_str()).ok())
            {
                servers.insert(state_key_uid.server_name().to_owned());
            }
        }

        // Remove our server from the server list since it will be added to it by room_servers() and/or the if statement above
        servers.remove(services().globals.server_name());

        services().sending.send_pdu(servers.into_iter(), &pdu_id)?;

        Ok(pdu.event_id)
    }

    /// Append the incoming event setting the state snapshot to the state from the
    /// server that sent the event.
    #[tracing::instrument(skip_all)]
    pub async fn append_incoming_pdu<'a>(
        &self,
        pdu: &PduEvent,
        pdu_json: CanonicalJsonObject,
        new_room_leaves: Vec<OwnedEventId>,
        state_ids_compressed: Arc<HashSet<CompressedStateEvent>>,
        soft_fail: bool,
        state_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<Option<Vec<u8>>> {
        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        services().rooms.state.set_event_state(
            &pdu.event_id,
            &pdu.room_id,
            state_ids_compressed,
        )?;

        if soft_fail {
            services()
                .rooms
                .pdu_metadata
                .mark_as_referenced(&pdu.room_id, &pdu.prev_events)?;
            services().rooms.state.set_forward_extremities(
                &pdu.room_id,
                new_room_leaves,
                state_lock,
            )?;
            return Ok(None);
        }

        let pdu_id = services()
            .rooms
            .timeline
            .append_pdu(pdu, pdu_json, new_room_leaves, state_lock)
            .await?;

        Ok(Some(pdu_id))
    }

    /// Returns an iterator over all PDUs in a room.
    pub fn all_pdus<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<impl Iterator<Item = Result<(PduCount, PduEvent)>> + 'a> {
        self.pdus_after(user_id, room_id, PduCount::min())
    }

    /// Returns an iterator over all events and their tokens in a room that happened before the
    /// event with id `until` in reverse-chronological order.
    #[tracing::instrument(skip(self))]
    pub fn pdus_until<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        until: PduCount,
    ) -> Result<impl Iterator<Item = Result<(PduCount, PduEvent)>> + 'a> {
        self.db.pdus_until(user_id, room_id, until)
    }

    /// Returns an iterator over all events and their token in a room that happened after the event
    /// with id `from` in chronological order.
    #[tracing::instrument(skip(self))]
    pub fn pdus_after<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        from: PduCount,
    ) -> Result<impl Iterator<Item = Result<(PduCount, PduEvent)>> + 'a> {
        self.db.pdus_after(user_id, room_id, from)
    }

    /// Replace a PDU with the redacted form.
    #[tracing::instrument(skip(self, reason))]
    pub fn redact_pdu(
        &self,
        event_id: &EventId,
        reason: &PduEvent,
        shortroomid: u64,
    ) -> Result<()> {
        // TODO: Don't reserialize, keep original json
        if let Some(pdu_id) = self.get_pdu_id(event_id)? {
            let mut pdu = self
                .get_pdu_from_id(&pdu_id)?
                .ok_or_else(|| Error::bad_database("PDU ID points to invalid PDU."))?;

            #[derive(Deserialize)]
            struct ExtractBody {
                body: String,
            }

            if let Ok(content) = serde_json::from_str::<ExtractBody>(pdu.content.get()) {
                services()
                    .rooms
                    .search
                    .deindex_pdu(shortroomid, &pdu_id, &content.body)?;
            }

            let room_version_id = services().rooms.state.get_room_version(&pdu.room_id)?;
            pdu.redact(room_version_id, reason)?;

            self.replace_pdu(
                &pdu_id,
                &utils::to_canonical_object(&pdu).expect("PDU is an object"),
                &pdu,
            )?;
        }
        // If event does not exist, just noop
        Ok(())
    }

    #[tracing::instrument(skip(self, room_id))]
    pub async fn backfill_if_required(&self, room_id: &RoomId, from: PduCount) -> Result<()> {
        let first_pdu = self
            .all_pdus(user_id!("@doesntmatter:conduit.rs"), room_id)?
            .next()
            .expect("Room is not empty")?;

        if first_pdu.0 < from {
            // No backfill required, there are still events between them
            return Ok(());
        }

        let power_levels: RoomPowerLevelsEventContent = services()
            .rooms
            .state_accessor
            .room_state_get(room_id, &StateEventType::RoomPowerLevels, "")?
            .map(|ev| {
                serde_json::from_str(ev.content.get())
                    .map_err(|_| Error::bad_database("invalid m.room.power_levels event"))
            })
            .transpose()?
            .unwrap_or_default();
        let mut admin_servers = power_levels
            .users
            .iter()
            .filter(|(_, level)| **level > power_levels.users_default)
            .map(|(user_id, _)| user_id.server_name())
            .collect::<HashSet<_>>();
        admin_servers.remove(services().globals.server_name());

        // Request backfill
        for backfill_server in admin_servers {
            info!("Asking {backfill_server} for backfill");
            let response = services()
                .sending
                .send_federation_request(
                    backfill_server,
                    federation::backfill::get_backfill::v1::Request {
                        room_id: room_id.to_owned(),
                        v: vec![first_pdu.1.event_id.as_ref().to_owned()],
                        limit: uint!(100),
                    },
                )
                .await;
            match response {
                Ok(response) => {
                    let pub_key_map = RwLock::new(BTreeMap::new());
                    for pdu in response.pdus {
                        if let Err(e) = self.backfill_pdu(backfill_server, pdu, &pub_key_map).await
                        {
                            warn!("Failed to add backfilled pdu: {e}");
                        }
                    }
                    return Ok(());
                }
                Err(e) => {
                    warn!("{backfill_server} could not provide backfill: {e}");
                }
            }
        }

        info!("No servers could backfill");
        Ok(())
    }

    #[tracing::instrument(skip(self, pdu))]
    pub async fn backfill_pdu(
        &self,
        origin: &ServerName,
        pdu: Box<RawJsonValue>,
        pub_key_map: &RwLock<BTreeMap<String, SigningKeys>>,
    ) -> Result<()> {
        let (event_id, value, room_id) = server_server::parse_incoming_pdu(&pdu)?;

        // Lock so we cannot backfill the same pdu twice at the same time
        let mutex = Arc::clone(
            services()
                .globals
                .roomid_mutex_federation
                .write()
                .await
                .entry(room_id.to_owned())
                .or_default(),
        );
        let mutex_lock = mutex.lock().await;

        // Skip the PDU if we already have it as a timeline event
        if let Some(pdu_id) = services().rooms.timeline.get_pdu_id(&event_id)? {
            info!("We already know {event_id} at {pdu_id:?}");
            return Ok(());
        }

        services()
            .rooms
            .event_handler
            .handle_incoming_pdu(origin, &event_id, &room_id, value, false, pub_key_map)
            .await?;

        let value = self.get_pdu_json(&event_id)?.expect("We just created it");
        let pdu = self.get_pdu(&event_id)?.expect("We just created it");

        let shortroomid = services()
            .rooms
            .short
            .get_shortroomid(&room_id)?
            .expect("room exists");

        let mutex_insert = Arc::clone(
            services()
                .globals
                .roomid_mutex_insert
                .write()
                .await
                .entry(room_id.clone())
                .or_default(),
        );
        let insert_lock = mutex_insert.lock().await;

        let count = services().globals.next_count()?;
        let mut pdu_id = shortroomid.to_be_bytes().to_vec();
        pdu_id.extend_from_slice(&0_u64.to_be_bytes());
        pdu_id.extend_from_slice(&(u64::MAX - count).to_be_bytes());

        // Insert pdu
        self.db.prepend_backfill_pdu(&pdu_id, &event_id, &value)?;

        drop(insert_lock);

        if pdu.kind == TimelineEventType::RoomMessage {
            #[derive(Deserialize)]
            struct ExtractBody {
                body: Option<String>,
            }

            let content = serde_json::from_str::<ExtractBody>(pdu.content.get())
                .map_err(|_| Error::bad_database("Invalid content in pdu."))?;

            if let Some(body) = content.body {
                services()
                    .rooms
                    .search
                    .index_pdu(shortroomid, &pdu_id, &body)?;
            }
        }
        drop(mutex_lock);

        info!("Prepended backfill pdu");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comparisons() {
        assert!(PduCount::Normal(1) < PduCount::Normal(2));
        assert!(PduCount::Backfilled(2) < PduCount::Backfilled(1));
        assert!(PduCount::Normal(1) > PduCount::Backfilled(1));
        assert!(PduCount::Backfilled(1) < PduCount::Normal(1));
    }
}
