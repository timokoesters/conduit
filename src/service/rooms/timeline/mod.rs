mod data;

use std::collections::HashMap;

use std::sync::{Arc, Mutex};
use std::{collections::HashSet};

pub use data::Data;
use regex::Regex;
use ruma::events::room::power_levels::RoomPowerLevelsEventContent;
use ruma::push::Ruleset;
use ruma::signatures::CanonicalJsonValue;
use ruma::state_res::RoomVersion;
use ruma::{
    api::client::error::ErrorKind,
    events::{
        push_rules::PushRulesEvent,
        room::{create::RoomCreateEventContent, member::MembershipState},
        GlobalAccountDataEventType, RoomEventType, StateEventType,
    },
    push::{Action, Tweak},
    serde::to_canonical_value,
    signatures::CanonicalJsonObject,
    state_res, uint, EventId, RoomAliasId, RoomId, ServerName, UserId,
};
use serde::Deserialize;
use serde_json::value::to_raw_value;
use tokio::sync::MutexGuard;
use tracing::{error, warn};

use crate::{
    service::pdu::{EventHash, PduBuilder},
    services, utils, Error, PduEvent, Result,
};

use super::state_compressor::CompressedStateEvent;

pub struct Service {
    pub db: &'static dyn Data,

    pub lasttimelinecount_cache: Mutex<HashMap<Box<RoomId>, u64>>,
}

impl Service {
    #[tracing::instrument(skip(self))]
    pub fn first_pdu_in_room(&self, room_id: &RoomId) -> Result<Option<Arc<PduEvent>>> {
        self.db.first_pdu_in_room(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn last_timeline_count(&self, sender_user: &UserId, room_id: &RoomId) -> Result<u64> {
        self.db.last_timeline_count(sender_user, room_id)
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

    /// Returns the `count` of this pdu's id.
    pub fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.db.get_pdu_count(event_id)
    }

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

    /// Returns the `count` of this pdu's id.
    pub fn pdu_count(&self, pdu_id: &[u8]) -> Result<u64> {
        self.db.pdu_count(pdu_id)
    }

    /// Removes a pdu and creates a new one with the same id.
    #[tracing::instrument(skip(self))]
    fn replace_pdu(&self, pdu_id: &[u8], pdu: &PduEvent) -> Result<()> {
        self.db.replace_pdu(pdu_id, pdu)
    }

    /// Creates a new persisted data unit and adds it to a room.
    ///
    /// By this point the incoming event should be fully authenticated, no auth happens
    /// in `append_pdu`.
    ///
    /// Returns pdu id
    #[tracing::instrument(skip(self, pdu, pdu_json, leaves))]
    pub fn append_pdu<'a>(
        &self,
        pdu: &PduEvent,
        mut pdu_json: CanonicalJsonObject,
        leaves: Vec<Box<EventId>>,
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
                .unwrap()
                .entry(pdu.room_id.clone())
                .or_default(),
        );
        let insert_lock = mutex_insert.lock().unwrap();

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

        for user in services()
            .rooms
            .state_cache
            .get_our_real_users(&pdu.room_id)?
            .iter()
        {
            // Don't notify the user of their own events
            if user == &pdu.sender {
                continue;
            }

            let rules_for_user = services()
                .account_data
                .get(
                    None,
                    &user,
                    GlobalAccountDataEventType::PushRules.to_string().into(),
                )?
                .map(|event| {
                    serde_json::from_str::<PushRulesEvent>(event.get())
                        .map_err(|_| Error::bad_database("Invalid push rules event in db."))
                })
                .transpose()?
                .map(|ev: PushRulesEvent| ev.content.global)
                .unwrap_or_else(|| Ruleset::server_default(&user));

            let mut highlight = false;
            let mut notify = false;

            for action in services().pusher.get_actions(
                &user,
                &rules_for_user,
                &power_levels,
                &sync_pdu,
                &pdu.room_id,
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

            if notify {
                notifies.push(user.clone());
            }

            if highlight {
                highlights.push(user.clone());
            }

            for push_key in services().pusher.get_pushkeys(&user) {
                services().sending.send_push_pdu(&*pdu_id, &user, push_key?)?;
            }
        }

        self.db
            .increment_notification_counts(&pdu.room_id, notifies, highlights)?;

        match pdu.kind {
            RoomEventType::RoomRedaction => {
                if let Some(redact_id) = &pdu.redacts {
                    self.redact_pdu(redact_id, pdu)?;
                }
            }
            RoomEventType::RoomMember => {
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

                    let invite_state = match content.membership {
                        MembershipState::Invite => {
                            let state = services().rooms.state.calculate_invite_state(pdu)?;
                            Some(state)
                        }
                        _ => None,
                    };

                    // Update our membership info, we do this here incase a user is invited
                    // and immediately leaves we need the DB to record the invite event for auth
                    services().rooms.state_cache.update_membership(
                        &pdu.room_id,
                        &target_user_id,
                        content.membership,
                        &pdu.sender,
                        invite_state,
                        true,
                    )?;
                }
            }
            RoomEventType::RoomMessage => {
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

                    let admin_room = services().rooms.alias.resolve_local_alias(
                        <&RoomAliasId>::try_from(
                            format!("#admins:{}", services().globals.server_name()).as_str(),
                        )
                        .expect("#admins:server_name is a valid room alias"),
                    )?;
                    let server_user = format!("@conduit:{}", services().globals.server_name());

                    let to_conduit = body.starts_with(&format!("{}: ", server_user));

                    // This will evaluate to false if the emergency password is set up so that
                    // the administrator can execute commands as conduit
                    let from_conduit = pdu.sender == server_user
                        && services().globals.emergency_password().is_none();

                    if to_conduit && !from_conduit && admin_room.as_ref() == Some(&pdu.room_id) {
                        services().admin.process_message(body.to_string());
                    }
                }
            }
            _ => {}
        }

        for appservice in services().appservice.all()? {
            if services()
                .rooms
                .state_cache
                .appservice_in_room(&pdu.room_id, &appservice)?
            {
                services()
                    .sending
                    .send_pdu_appservice(appservice.0, pdu_id.clone())?;
                continue;
            }

            // If the RoomMember event has a non-empty state_key, it is targeted at someone.
            // If it is our appservice user, we send this PDU to it.
            if pdu.kind == RoomEventType::RoomMember {
                if let Some(state_key_uid) = &pdu
                    .state_key
                    .as_ref()
                    .and_then(|state_key| UserId::parse(state_key.as_str()).ok())
                {
                    if let Some(appservice_uid) = appservice
                        .1
                        .get("sender_localpart")
                        .and_then(|string| string.as_str())
                        .and_then(|string| {
                            UserId::parse_with_server_name(string, services().globals.server_name())
                                .ok()
                        })
                    {
                        if state_key_uid == &appservice_uid {
                            services()
                                .sending
                                .send_pdu_appservice(appservice.0, pdu_id.clone())?;
                            continue;
                        }
                    }
                }
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
                        || pdu.kind == RoomEventType::RoomMember
                            && pdu
                                .state_key
                                .as_ref()
                                .map_or(false, |state_key| users.is_match(state_key))
                };
                let matching_aliases = |aliases: &Regex| {
                    services()
                        .rooms
                        .alias
                        .local_aliases_for_room(&pdu.room_id)
                        .filter_map(|r| r.ok())
                        .any(|room_alias| aliases.is_match(room_alias.as_str()))
                };

                if aliases.iter().any(matching_aliases)
                    || rooms.map_or(false, |rooms| rooms.contains(&pdu.room_id.as_str().into()))
                    || users.iter().any(matching_users)
                {
                    services()
                        .sending
                        .send_pdu_appservice(appservice.0, pdu_id.clone())?;
                }
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
        } = pdu_builder;

        let prev_events: Vec<_> = services()
            .rooms
            .state
            .get_forward_extremities(room_id)?
            .into_iter()
            .take(20)
            .collect();

        let create_event = services().rooms.state_accessor.room_state_get(
            room_id,
            &StateEventType::RoomCreate,
            "",
        )?;

        let create_event_content: Option<RoomCreateEventContent> = create_event
            .as_ref()
            .map(|create_event| {
                serde_json::from_str(create_event.content.get()).map_err(|e| {
                    warn!("Invalid create event: {}", e);
                    Error::bad_database("Invalid create event in db.")
                })
            })
            .transpose()?;

        // If there was no create event yet, assume we are creating a room with the default
        // version right now
        let room_version_id = create_event_content
            .map_or(services().globals.default_room_version(), |create_event| {
                create_event.room_version
            });
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

        let auth_check = state_res::auth_check(
            &room_version,
            &pdu,
            None::<PduEvent>, // TODO: third_party_invite
            |k, s| auth_events.get(&(k.clone(), s.to_owned())),
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
                .expect("ruma can calculate reference hashes")
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
    pub fn build_and_append_pdu(
        &self,
        pdu_builder: PduBuilder,
        sender: &UserId,
        room_id: &RoomId,
        state_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<Arc<EventId>> {
        let (pdu, pdu_json) =
            self.create_hash_and_sign_event(pdu_builder, sender, room_id, &state_lock)?;

        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        let statehashid = services().rooms.state.append_to_state(&pdu)?;

        let pdu_id = self.append_pdu(
            &pdu,
            pdu_json,
            // Since this PDU references all pdu_leaves we can update the leaves
            // of the room
            vec![(*pdu.event_id).to_owned()],
            state_lock,
        )?;

        // We set the room state after inserting the pdu, so that we never have a moment in time
        // where events in the current room state do not exist
        services()
            .rooms
            .state
            .set_room_state(room_id, statehashid, state_lock)?;

        let mut servers: HashSet<Box<ServerName>> = services()
            .rooms
            .state_cache
            .room_servers(room_id)
            .filter_map(|r| r.ok())
            .collect();

        // In case we are kicking or banning a user, we need to inform their server of the change
        if pdu.kind == RoomEventType::RoomMember {
            if let Some(state_key_uid) = &pdu
                .state_key
                .as_ref()
                .and_then(|state_key| UserId::parse(state_key.as_str()).ok())
            {
                servers.insert(Box::from(state_key_uid.server_name()));
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
    pub fn append_incoming_pdu<'a>(
        &self,
        pdu: &PduEvent,
        pdu_json: CanonicalJsonObject,
        new_room_leaves: Vec<Box<EventId>>,
        state_ids_compressed: HashSet<CompressedStateEvent>,
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

        let pdu_id =
            services()
                .rooms
                .timeline
                .append_pdu(pdu, pdu_json, new_room_leaves, state_lock)?;

        Ok(Some(pdu_id))
    }

    /// Returns an iterator over all PDUs in a room.
    pub fn all_pdus<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<impl Iterator<Item = Result<(Vec<u8>, PduEvent)>> + 'a> {
        self.pdus_since(user_id, room_id, 0)
    }

    /// Returns an iterator over all events in a room that happened after the event with id `since`
    /// in chronological order.
    pub fn pdus_since<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<impl Iterator<Item = Result<(Vec<u8>, PduEvent)>> + 'a> {
        self.db.pdus_since(user_id, room_id, since)
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
        self.db.pdus_until(user_id, room_id, until)
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
        self.db.pdus_after(user_id, room_id, from)
    }

    /// Replace a PDU with the redacted form.
    #[tracing::instrument(skip(self, reason))]
    pub fn redact_pdu(&self, event_id: &EventId, reason: &PduEvent) -> Result<()> {
        if let Some(pdu_id) = self.get_pdu_id(event_id)? {
            let mut pdu = self
                .get_pdu_from_id(&pdu_id)?
                .ok_or_else(|| Error::bad_database("PDU ID points to invalid PDU."))?;
            pdu.redact(reason)?;
            self.replace_pdu(&pdu_id, &pdu)?;
        }
        // If event does not exist, just noop
        Ok(())
    }
}
