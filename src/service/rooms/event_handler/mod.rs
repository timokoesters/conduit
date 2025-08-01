/// An async function that can recursively call itself.
type AsyncRecursiveType<'a, T> = Pin<Box<dyn Future<Output = T> + 'a + Send>>;

use std::{
    collections::{hash_map, BTreeMap, HashMap, HashSet},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use futures_util::{stream::FuturesUnordered, Future, StreamExt};
use globals::SigningKeys;
use ruma::{
    api::{
        client::error::ErrorKind,
        federation::{
            discovery::{
                get_remote_server_keys,
                get_remote_server_keys_batch::{self, v2::QueryCriteria},
                get_server_keys,
            },
            event::{get_event, get_room_state_ids},
            membership::create_join_event,
        },
    },
    events::{
        room::{
            create::RoomCreateEventContent, redaction::RoomRedactionEventContent,
            server_acl::RoomServerAclEventContent,
        },
        StateEventType, TimelineEventType,
    },
    int,
    room_version_rules::{AuthorizationRules, RoomVersionRules},
    state_res::{self, StateMap},
    uint, CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch,
    OwnedServerName, OwnedServerSigningKeyId, RoomId, ServerName,
};
use serde_json::value::RawValue as RawJsonValue;
use tokio::sync::{RwLock, RwLockWriteGuard, Semaphore};
use tracing::{debug, error, info, trace, warn};

use crate::{service::*, services, Error, PduEvent, Result};

use super::state_compressor::CompressedStateEvent;

pub struct Service;

impl Service {
    /// When receiving an event one needs to:
    /// 0. Check the server is in the room
    /// 1. Skip the PDU if we already know about it
    /// 1.1. Remove unsigned field
    /// 2. Check event is valid, otherwise drop
    /// 3. Check signatures, otherwise drop
    /// 4. Check content hash, redact if doesn't match
    /// 5. Fetch any missing auth events doing all checks listed here starting at 1. These are not
    ///    timeline events
    /// 6. Reject "due to auth events" if can't get all the auth events or some of the auth events are
    ///    also rejected "due to auth events"
    /// 7. Reject "due to auth events" if the event doesn't pass auth based on the auth events
    /// 8. Persist this event as an outlier
    /// 9. If not timeline event: stop
    /// 10. Fetch any missing prev events doing all checks listed here starting at 1. These are timeline
    ///    events
    /// 11. Fetch missing state and auth chain events by calling /state_ids at backwards extremities
    ///     doing all the checks in this list starting at 1. These are not timeline events
    /// 12. Check the auth of the event passes based on the state of the event
    /// 13. Ensure that the state is derived from the previous current state (i.e. we calculated by
    ///     doing state res where one of the inputs was a previously trusted set of state, don't just
    ///     trust a set of state we got from a remote)
    /// 14. Use state resolution to find new room state
    /// 15. Check if the event passes auth based on the "current state" of the room, if not soft fail it
    // We use some AsyncRecursiveType hacks here so we can call this async function recursively
    #[tracing::instrument(skip(self, value, is_timeline_event, pub_key_map))]
    pub(crate) async fn handle_incoming_pdu<'a>(
        &self,
        origin: &'a ServerName,
        event_id: &'a EventId,
        room_id: &'a RoomId,
        value: BTreeMap<String, CanonicalJsonValue>,
        is_timeline_event: bool,
        pub_key_map: &'a RwLock<BTreeMap<String, SigningKeys>>,
    ) -> Result<Option<Vec<u8>>> {
        // 0. Check the server is in the room
        if !services().rooms.metadata.exists(room_id)? {
            return Err(Error::BadRequest(
                ErrorKind::NotFound,
                "Room is unknown to this server",
            ));
        }

        if services().rooms.metadata.is_disabled(room_id)? {
            return Err(Error::BadRequest(
                ErrorKind::forbidden(),
                "Federation of this room is currently disabled on this server.",
            ));
        }

        services().rooms.event_handler.acl_check(origin, room_id)?;

        // 1. Skip the PDU if we already have it as a timeline event
        if let Some(pdu_id) = services().rooms.timeline.get_pdu_id(event_id)? {
            return Ok(Some(pdu_id.to_vec()));
        }

        let create_event = services()
            .rooms
            .state_accessor
            .room_state_get(room_id, &StateEventType::RoomCreate, "")?
            .ok_or_else(|| Error::bad_database("Failed to find create event in db."))?;

        let create_event_content: RoomCreateEventContent =
            serde_json::from_str(create_event.content.get()).map_err(|e| {
                error!("Invalid create event: {}", e);
                Error::BadDatabase("Invalid create event in db")
            })?;
        let room_version_id = &create_event_content.room_version;

        let first_pdu_in_room = services()
            .rooms
            .timeline
            .first_pdu_in_room(room_id)?
            .ok_or_else(|| Error::bad_database("Failed to find first pdu in db."))?;

        let (incoming_pdu, val) = self
            .handle_outlier_pdu(
                origin,
                &create_event,
                event_id,
                room_id,
                value,
                false,
                pub_key_map,
            )
            .await?;
        self.check_room_id(room_id, &incoming_pdu)?;

        // 9. if not timeline event: stop
        if !is_timeline_event {
            return Ok(None);
        }

        // Skip old events
        if incoming_pdu.origin_server_ts < first_pdu_in_room.origin_server_ts {
            return Ok(None);
        }

        // 10. Fetch any missing prev events doing all checks listed here starting at 1. These are timeline events
        let (sorted_prev_events, mut eventid_info) = self
            .fetch_unknown_prev_events(
                origin,
                &create_event,
                room_id,
                &room_version_id
                    .rules()
                    .expect("Supported room version has rules"),
                pub_key_map,
                incoming_pdu.prev_events.clone(),
            )
            .await?;

        let mut errors = 0;
        debug!(events = ?sorted_prev_events, "Got previous events");
        for prev_id in sorted_prev_events {
            // Check for disabled again because it might have changed
            if services().rooms.metadata.is_disabled(room_id)? {
                return Err(Error::BadRequest(
                    ErrorKind::forbidden(),
                    "Federation of this room is currently disabled on this server.",
                ));
            }

            if let Some((time, tries)) = services()
                .globals
                .bad_event_ratelimiter
                .read()
                .await
                .get(&*prev_id)
            {
                // Exponential backoff
                let mut min_elapsed_duration = Duration::from_secs(5 * 60) * (*tries) * (*tries);
                if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
                    min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
                }

                if time.elapsed() < min_elapsed_duration {
                    info!("Backing off from {}", prev_id);
                    continue;
                }
            }

            if errors >= 5 {
                // Timeout other events
                match services()
                    .globals
                    .bad_event_ratelimiter
                    .write()
                    .await
                    .entry((*prev_id).to_owned())
                {
                    hash_map::Entry::Vacant(e) => {
                        e.insert((Instant::now(), 1));
                    }
                    hash_map::Entry::Occupied(mut e) => {
                        *e.get_mut() = (Instant::now(), e.get().1 + 1)
                    }
                }
                continue;
            }

            if let Some((pdu, json)) = eventid_info.remove(&*prev_id) {
                // Skip old events
                if pdu.origin_server_ts < first_pdu_in_room.origin_server_ts {
                    continue;
                }

                let start_time = Instant::now();
                services()
                    .globals
                    .roomid_federationhandletime
                    .write()
                    .await
                    .insert(room_id.to_owned(), ((*prev_id).to_owned(), start_time));

                if let Err(e) = self
                    .upgrade_outlier_to_timeline_pdu(
                        pdu,
                        json,
                        &create_event,
                        origin,
                        room_id,
                        pub_key_map,
                    )
                    .await
                {
                    errors += 1;
                    warn!("Prev event {} failed: {}", prev_id, e);
                    match services()
                        .globals
                        .bad_event_ratelimiter
                        .write()
                        .await
                        .entry((*prev_id).to_owned())
                    {
                        hash_map::Entry::Vacant(e) => {
                            e.insert((Instant::now(), 1));
                        }
                        hash_map::Entry::Occupied(mut e) => {
                            *e.get_mut() = (Instant::now(), e.get().1 + 1)
                        }
                    }
                }
                let elapsed = start_time.elapsed();
                services()
                    .globals
                    .roomid_federationhandletime
                    .write()
                    .await
                    .remove(&room_id.to_owned());
                debug!(
                    "Handling prev event {} took {}m{}s",
                    prev_id,
                    elapsed.as_secs() / 60,
                    elapsed.as_secs() % 60
                );
            }
        }

        // Done with prev events, now handling the incoming event

        let start_time = Instant::now();
        services()
            .globals
            .roomid_federationhandletime
            .write()
            .await
            .insert(room_id.to_owned(), (event_id.to_owned(), start_time));
        let r = services()
            .rooms
            .event_handler
            .upgrade_outlier_to_timeline_pdu(
                incoming_pdu,
                val,
                &create_event,
                origin,
                room_id,
                pub_key_map,
            )
            .await;
        services()
            .globals
            .roomid_federationhandletime
            .write()
            .await
            .remove(&room_id.to_owned());

        r
    }

    #[allow(clippy::type_complexity, clippy::too_many_arguments)]
    #[tracing::instrument(skip(self, create_event, value, pub_key_map))]
    fn handle_outlier_pdu<'a>(
        &'a self,
        origin: &'a ServerName,
        create_event: &'a PduEvent,
        event_id: &'a EventId,
        room_id: &'a RoomId,
        mut value: BTreeMap<String, CanonicalJsonValue>,
        auth_events_known: bool,
        pub_key_map: &'a RwLock<BTreeMap<String, SigningKeys>>,
    ) -> AsyncRecursiveType<'a, Result<(Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>)>> {
        Box::pin(async move {
            // 1.1. Remove unsigned field
            value.remove("unsigned");

            // 2. Check event is valid, otherwise drop
            // 3. Check signatures, otherwise drop
            // 4. check content hash, redact if doesn't match
            let create_event_content: RoomCreateEventContent =
                serde_json::from_str(create_event.content.get()).map_err(|e| {
                    error!("Invalid create event: {}", e);
                    Error::BadDatabase("Invalid create event in db")
                })?;

            let room_version_id = &create_event_content.room_version;
            let room_version_rules = room_version_id
                .rules()
                .expect("Supported room version has rules");

            debug!("Checking format of join event PDU");
            if let Err(e) = state_res::check_pdu_format(&value, &room_version_rules.event_format) {
                warn!("Invalid PDU with event ID {event_id} received: {e}");
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Received Invalid PDU",
                ));
            }

            // TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json

            // We go through all the signatures we see on the value and fetch the corresponding signing
            // keys
            self.fetch_required_signing_keys(&value, pub_key_map)
                .await?;

            let origin_server_ts = value.get("origin_server_ts").ok_or_else(|| {
                error!("Invalid PDU, no origin_server_ts field");
                Error::BadRequest(
                    ErrorKind::MissingParam,
                    "Invalid PDU, no origin_server_ts field",
                )
            })?;

            let origin_server_ts: MilliSecondsSinceUnixEpoch = {
                let ts = origin_server_ts.as_integer().ok_or_else(|| {
                    Error::BadRequest(
                        ErrorKind::InvalidParam,
                        "origin_server_ts must be an integer",
                    )
                })?;

                MilliSecondsSinceUnixEpoch(i64::from(ts).try_into().map_err(|_| {
                    Error::BadRequest(ErrorKind::InvalidParam, "Time must be after the unix epoch")
                })?)
            };

            let guard = pub_key_map.read().await;

            let pkey_map = (*guard).clone();

            // Removing all the expired keys, unless the room version allows stale keys
            let filtered_keys = services().globals.filter_keys_server_map(
                pkey_map,
                origin_server_ts,
                &room_version_rules,
            );

            let mut val =
                match ruma::signatures::verify_event(&filtered_keys, &value, &room_version_rules) {
                    Err(e) => {
                        // Drop
                        warn!("Dropping bad event {}: {}", event_id, e,);
                        return Err(Error::BadRequest(
                            ErrorKind::InvalidParam,
                            "Signature verification failed",
                        ));
                    }
                    Ok(ruma::signatures::Verified::Signatures) => {
                        // Redact
                        warn!("Calculated hash does not match: {}", event_id);
                        let obj = match ruma::canonical_json::redact(
                            value,
                            &room_version_rules.redaction,
                            None,
                        ) {
                            Ok(obj) => obj,
                            Err(_) => {
                                return Err(Error::BadRequest(
                                    ErrorKind::InvalidParam,
                                    "Redaction failed",
                                ))
                            }
                        };

                        // Skip the PDU if it is redacted and we already have it as an outlier event
                        if services().rooms.timeline.get_pdu_json(event_id)?.is_some() {
                            return Err(Error::BadRequest(
                                ErrorKind::InvalidParam,
                                "Event was redacted and we already knew about it",
                            ));
                        }

                        obj
                    }
                    Ok(ruma::signatures::Verified::All) => value,
                };

            drop(guard);

            // Now that we have checked the signature and hashes we can add the eventID and convert
            // to our PduEvent type
            val.insert(
                "event_id".to_owned(),
                CanonicalJsonValue::String(event_id.as_str().to_owned()),
            );
            let incoming_pdu = serde_json::from_value::<PduEvent>(
                serde_json::to_value(&val).expect("CanonicalJsonObj is a valid JsonValue"),
            )
            .map_err(|_| Error::bad_database("Event is not a valid PDU."))?;

            self.check_room_id(room_id, &incoming_pdu)?;

            if !auth_events_known {
                // 5. fetch any missing auth events doing all checks listed here starting at 1. These are not timeline events
                // 6. Reject "due to auth events" if can't get all the auth events or some of the auth events are also rejected "due to auth events"
                // NOTE: Step 5 is not applied anymore because it failed too often
                debug!(event_id = ?incoming_pdu.event_id, "Fetching auth events");
                self.fetch_and_handle_outliers(
                    origin,
                    &incoming_pdu
                        .auth_events
                        .iter()
                        .map(|x| Arc::from(&**x))
                        .collect::<Vec<_>>(),
                    create_event,
                    room_id,
                    &room_version_rules,
                    pub_key_map,
                )
                .await;
            }

            // 7. Reject "due to auth events" if the event doesn't pass auth based on the auth events
            debug!(
                "Auth check for {} based on auth events",
                incoming_pdu.event_id
            );

            // Build map of auth events
            let mut auth_events = HashMap::new();
            let mut auth_events_by_event_id = HashMap::new();
            for id in &incoming_pdu.auth_events {
                let auth_event = match services().rooms.timeline.get_pdu(id)? {
                    Some(e) => e,
                    None => {
                        warn!("Could not find auth event {}", id);
                        continue;
                    }
                };

                auth_events_by_event_id.insert(auth_event.event_id.clone(), auth_event.clone());
                auth_events.insert(
                    (
                        StateEventType::from(auth_event.kind.to_string()),
                        auth_event
                            .state_key
                            .clone()
                            .expect("all auth events have state keys"),
                    ),
                    auth_event,
                );
            }

            // first time we are doing any sort of auth check, so we check state-independent
            // auth rules in addition to the state-dependent ones.
            if state_res::check_state_independent_auth_rules(
                &room_version_rules.authorization,
                &incoming_pdu,
                |event_id| auth_events_by_event_id.get(event_id),
            )
            .is_err()
                || state_res::check_state_dependent_auth_rules(
                    &room_version_rules.authorization,
                    &incoming_pdu,
                    |k, s| auth_events.get(&(k.to_string().into(), s.to_owned())),
                )
                .is_err()
            {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Auth check failed",
                ));
            }

            debug!("Validation successful.");

            // 8. Persist the event as an outlier.
            services()
                .rooms
                .outlier
                .add_pdu_outlier(&incoming_pdu.event_id, &val)?;

            debug!("Added pdu as outlier.");

            Ok((Arc::new(incoming_pdu), val))
        })
    }

    #[tracing::instrument(skip(self, incoming_pdu, val, create_event, pub_key_map))]
    pub async fn upgrade_outlier_to_timeline_pdu(
        &self,
        incoming_pdu: Arc<PduEvent>,
        val: BTreeMap<String, CanonicalJsonValue>,
        create_event: &PduEvent,
        origin: &ServerName,
        room_id: &RoomId,
        pub_key_map: &RwLock<BTreeMap<String, SigningKeys>>,
    ) -> Result<Option<Vec<u8>>> {
        // Skip the PDU if we already have it as a timeline event
        if let Ok(Some(pduid)) = services().rooms.timeline.get_pdu_id(&incoming_pdu.event_id) {
            return Ok(Some(pduid));
        }

        if services()
            .rooms
            .pdu_metadata
            .is_event_soft_failed(&incoming_pdu.event_id)?
        {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event has been soft failed",
            ));
        }

        info!("Upgrading {} to timeline pdu", incoming_pdu.event_id);

        let create_event_content: RoomCreateEventContent =
            serde_json::from_str(create_event.content.get()).map_err(|e| {
                warn!("Invalid create event: {}", e);
                Error::BadDatabase("Invalid create event in db")
            })?;

        let room_version_id = &create_event_content.room_version;
        let room_version_rules = room_version_id
            .rules()
            .expect("Supported room version has rules");

        // 11. Fetch missing state and auth chain events by calling /state_ids at backwards extremities
        //     doing all the checks in this list starting at 1. These are not timeline events.

        // TODO: if we know the prev_events of the incoming event we can avoid the request and build
        // the state from a known point and resolve if > 1 prev_event

        debug!("Requesting state at event");
        let mut state_at_incoming_event = None;

        if incoming_pdu.prev_events.len() == 1 {
            let prev_event = &*incoming_pdu.prev_events[0];
            let prev_event_sstatehash = services()
                .rooms
                .state_accessor
                .pdu_shortstatehash(prev_event)?;

            let state = if let Some(shortstatehash) = prev_event_sstatehash {
                Some(
                    services()
                        .rooms
                        .state_accessor
                        .state_full_ids(shortstatehash)
                        .await,
                )
            } else {
                None
            };

            if let Some(Ok(mut state)) = state {
                debug!("Using cached state");
                let prev_pdu = services()
                    .rooms
                    .timeline
                    .get_pdu(prev_event)
                    .ok()
                    .flatten()
                    .ok_or_else(|| {
                        Error::bad_database("Could not find prev event, but we know the state.")
                    })?;

                if let Some(state_key) = &prev_pdu.state_key {
                    let shortstatekey = services().rooms.short.get_or_create_shortstatekey(
                        &prev_pdu.kind.to_string().into(),
                        state_key,
                    )?;

                    state.insert(shortstatekey, Arc::from(prev_event));
                    // Now it's the state after the pdu
                }

                state_at_incoming_event = Some(state);
            }
        } else {
            debug!("Calculating state at event using state res");
            let mut extremity_sstatehashes = HashMap::new();

            let mut okay = true;
            for prev_eventid in &incoming_pdu.prev_events {
                let prev_event =
                    if let Ok(Some(pdu)) = services().rooms.timeline.get_pdu(prev_eventid) {
                        pdu
                    } else {
                        okay = false;
                        break;
                    };

                let sstatehash = if let Ok(Some(s)) = services()
                    .rooms
                    .state_accessor
                    .pdu_shortstatehash(prev_eventid)
                {
                    s
                } else {
                    okay = false;
                    break;
                };

                extremity_sstatehashes.insert(sstatehash, prev_event);
            }

            if okay {
                let mut fork_states = Vec::with_capacity(extremity_sstatehashes.len());
                let mut auth_chain_sets = Vec::with_capacity(extremity_sstatehashes.len());

                for (sstatehash, prev_event) in extremity_sstatehashes {
                    let mut leaf_state: HashMap<_, _> = services()
                        .rooms
                        .state_accessor
                        .state_full_ids(sstatehash)
                        .await?;

                    if let Some(state_key) = &prev_event.state_key {
                        let shortstatekey = services().rooms.short.get_or_create_shortstatekey(
                            &prev_event.kind.to_string().into(),
                            state_key,
                        )?;
                        leaf_state.insert(shortstatekey, Arc::from(&*prev_event.event_id));
                        // Now it's the state after the pdu
                    }

                    let mut state = StateMap::with_capacity(leaf_state.len());
                    let mut starting_events = Vec::with_capacity(leaf_state.len());

                    for (k, id) in leaf_state {
                        if let Ok((ty, st_key)) = services().rooms.short.get_statekey_from_short(k)
                        {
                            // FIXME: Undo .to_string().into() when StateMap
                            //        is updated to use StateEventType
                            state.insert((ty.to_string().into(), st_key), id.clone());
                        } else {
                            warn!("Failed to get_statekey_from_short.");
                        }
                        starting_events.push(id);
                    }

                    auth_chain_sets.push(
                        services()
                            .rooms
                            .auth_chain
                            .get_auth_chain(room_id, starting_events)
                            .await?
                            .collect(),
                    );

                    fork_states.push(state);
                }

                let lock = services().globals.stateres_mutex.lock();

                let result = state_res::resolve(
                    &room_version_id
                        .rules()
                        .expect("Supported room version has rules")
                        .authorization,
                    &fork_states,
                    auth_chain_sets,
                    |id| {
                        let res = services().rooms.timeline.get_pdu(id);
                        if let Err(e) = &res {
                            error!("LOOK AT ME Failed to fetch event: {}", e);
                        }
                        res.ok().flatten()
                    },
                );
                drop(lock);

                state_at_incoming_event = match result {
                    Ok(new_state) => Some(
                        new_state
                            .into_iter()
                            .map(|((event_type, state_key), event_id)| {
                                let shortstatekey =
                                    services().rooms.short.get_or_create_shortstatekey(
                                        &event_type.to_string().into(),
                                        &state_key,
                                    )?;
                                Ok((shortstatekey, event_id))
                            })
                            .collect::<Result<_>>()?,
                    ),
                    Err(e) => {
                        warn!("State resolution on prev events failed, either an event could not be found or deserialization: {}", e);
                        None
                    }
                }
            }
        }

        if state_at_incoming_event.is_none() {
            debug!("Calling /state_ids");
            // Call /state_ids to find out what the state at this pdu is. We trust the server's
            // response to some extend, but we still do a lot of checks on the events
            match services()
                .sending
                .send_federation_request(
                    origin,
                    get_room_state_ids::v1::Request {
                        room_id: room_id.to_owned(),
                        event_id: (*incoming_pdu.event_id).to_owned(),
                    },
                )
                .await
            {
                Ok(res) => {
                    debug!("Fetching state events at event.");
                    let collect = res
                        .pdu_ids
                        .iter()
                        .map(|x| Arc::from(&**x))
                        .collect::<Vec<_>>();
                    let state_vec = self
                        .fetch_and_handle_outliers(
                            origin,
                            &collect,
                            create_event,
                            room_id,
                            &room_version_rules,
                            pub_key_map,
                        )
                        .await;

                    let mut state: HashMap<_, Arc<EventId>> = HashMap::new();
                    for (pdu, _) in state_vec {
                        let state_key = pdu.state_key.clone().ok_or_else(|| {
                            Error::bad_database("Found non-state pdu in state events.")
                        })?;

                        let shortstatekey = services().rooms.short.get_or_create_shortstatekey(
                            &pdu.kind.to_string().into(),
                            &state_key,
                        )?;

                        match state.entry(shortstatekey) {
                            hash_map::Entry::Vacant(v) => {
                                v.insert(Arc::from(&*pdu.event_id));
                            }
                            hash_map::Entry::Occupied(_) => return Err(
                                Error::bad_database("State event's type and state_key combination exists multiple times."),
                            ),
                        }
                    }

                    // The original create event must still be in the state
                    let create_shortstatekey = services()
                        .rooms
                        .short
                        .get_shortstatekey(&StateEventType::RoomCreate, "")?
                        .expect("Room exists");

                    if state.get(&create_shortstatekey).map(|id| id.as_ref())
                        != Some(&create_event.event_id)
                    {
                        return Err(Error::bad_database(
                            "Incoming event refers to wrong create event.",
                        ));
                    }

                    state_at_incoming_event = Some(state);
                }
                Err(e) => {
                    warn!("Fetching state for event failed: {}", e);
                    return Err(e);
                }
            };
        }

        let state_at_incoming_event =
            state_at_incoming_event.expect("we always set this to some above");

        debug!("Starting auth check");
        // 12. Check the auth of the event passes based on the state of the event
        if state_res::check_state_dependent_auth_rules(
            &room_version_rules.authorization,
            &incoming_pdu,
            |k, s| {
                services()
                    .rooms
                    .short
                    .get_shortstatekey(&k.to_string().into(), s)
                    .ok()
                    .flatten()
                    .and_then(|shortstatekey| state_at_incoming_event.get(&shortstatekey))
                    .and_then(|event_id| services().rooms.timeline.get_pdu(event_id).ok().flatten())
            },
        )
        .is_err()
        {
            return Err(Error::bad_database(
                "Event has failed auth check with state at the event.",
            ));
        }
        debug!("Auth check succeeded");

        // Soft fail check before doing state res
        let auth_events = services().rooms.state.get_auth_events(
            room_id,
            &incoming_pdu.kind,
            &incoming_pdu.sender,
            incoming_pdu.state_key.as_deref(),
            &incoming_pdu.content,
            &room_version_rules.authorization,
        )?;

        let soft_fail = state_res::check_state_dependent_auth_rules(
            &room_version_rules.authorization,
            &incoming_pdu,
            |k, s| auth_events.get(&(k.clone(), s.to_owned())),
        )
        .is_err()
            || incoming_pdu.kind == TimelineEventType::RoomRedaction
                && if room_version_rules.redaction.content_field_redacts {
                    let content = serde_json::from_str::<RoomRedactionEventContent>(
                        incoming_pdu.content.get(),
                    )
                    .map_err(|_| Error::bad_database("Invalid content in redaction pdu."))?;

                    if let Some(redact_id) = &content.redacts {
                        !services().rooms.state_accessor.user_can_redact(
                            redact_id,
                            &incoming_pdu.sender,
                            &incoming_pdu.room_id,
                            true,
                        )?
                    } else {
                        false
                    }
                } else if let Some(redact_id) = &incoming_pdu.redacts {
                    !services().rooms.state_accessor.user_can_redact(
                        redact_id,
                        &incoming_pdu.sender,
                        &incoming_pdu.room_id,
                        true,
                    )?
                } else {
                    false
                };

        // 14. Use state resolution to find new room state

        // We start looking at current room state now, so lets lock the room
        let mutex_state = Arc::clone(
            services()
                .globals
                .roomid_mutex_state
                .write()
                .await
                .entry(room_id.to_owned())
                .or_default(),
        );
        let state_lock = mutex_state.lock().await;

        // Now we calculate the set of extremities this room has after the incoming event has been
        // applied. We start with the previous extremities (aka leaves)
        debug!("Calculating extremities");
        let mut extremities = services().rooms.state.get_forward_extremities(room_id)?;

        // Remove any forward extremities that are referenced by this incoming event's prev_events
        for prev_event in &incoming_pdu.prev_events {
            if extremities.contains(prev_event) {
                extremities.remove(prev_event);
            }
        }

        // Only keep those extremities were not referenced yet
        extremities.retain(|id| {
            !matches!(
                services()
                    .rooms
                    .pdu_metadata
                    .is_event_referenced(room_id, id),
                Ok(true)
            )
        });

        debug!("Compressing state at event");
        let state_ids_compressed = Arc::new(
            state_at_incoming_event
                .iter()
                .map(|(shortstatekey, id)| {
                    services()
                        .rooms
                        .state_compressor
                        .compress_state_event(*shortstatekey, id)
                })
                .collect::<Result<_>>()?,
        );

        if incoming_pdu.state_key.is_some() {
            debug!("Preparing for stateres to derive new room state");

            // We also add state after incoming event to the fork states
            let mut state_after = state_at_incoming_event.clone();
            if let Some(state_key) = &incoming_pdu.state_key {
                let shortstatekey = services().rooms.short.get_or_create_shortstatekey(
                    &incoming_pdu.kind.to_string().into(),
                    state_key,
                )?;

                state_after.insert(shortstatekey, Arc::from(&*incoming_pdu.event_id));
            }

            let new_room_state = self
                .resolve_state(room_id, &room_version_rules.authorization, state_after)
                .await?;

            // Set the new room state to the resolved state
            debug!("Forcing new room state");

            let (sstatehash, new, removed) = services()
                .rooms
                .state_compressor
                .save_state(room_id, new_room_state)?;

            services()
                .rooms
                .state
                .force_state(room_id, sstatehash, new, removed, &state_lock)
                .await?;
        }

        // 15. Check if the event passes auth based on the "current state" of the room, if not soft fail it
        debug!("Starting soft fail auth check");

        if soft_fail {
            services()
                .rooms
                .timeline
                .append_incoming_pdu(
                    &incoming_pdu,
                    val,
                    extremities.iter().map(|e| (**e).to_owned()).collect(),
                    state_ids_compressed,
                    soft_fail,
                    &state_lock,
                )
                .await?;

            // Soft fail, we keep the event as an outlier but don't add it to the timeline
            warn!("Event was soft failed: {:?}", incoming_pdu);
            services()
                .rooms
                .pdu_metadata
                .mark_event_soft_failed(&incoming_pdu.event_id)?;
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event has been soft failed",
            ));
        }

        debug!("Appending pdu to timeline");
        extremities.insert(incoming_pdu.event_id.clone());

        // Now that the event has passed all auth it is added into the timeline.
        // We use the `state_at_event` instead of `state_after` so we accurately
        // represent the state for this event.

        let pdu_id = services()
            .rooms
            .timeline
            .append_incoming_pdu(
                &incoming_pdu,
                val,
                extremities.iter().map(|e| (**e).to_owned()).collect(),
                state_ids_compressed,
                soft_fail,
                &state_lock,
            )
            .await?;

        debug!("Appended incoming pdu");

        // Event has passed all auth/stateres checks
        drop(state_lock);
        Ok(pdu_id)
    }

    async fn resolve_state(
        &self,
        room_id: &RoomId,
        auth_rules: &AuthorizationRules,
        incoming_state: HashMap<u64, Arc<EventId>>,
    ) -> Result<Arc<HashSet<CompressedStateEvent>>> {
        debug!("Loading current room state ids");
        let current_sstatehash = services()
            .rooms
            .state
            .get_room_shortstatehash(room_id)?
            .expect("every room has state");

        let current_state_ids = services()
            .rooms
            .state_accessor
            .state_full_ids(current_sstatehash)
            .await?;

        let fork_states = [current_state_ids, incoming_state];

        let mut auth_chain_sets = Vec::new();
        for state in &fork_states {
            auth_chain_sets.push(
                services()
                    .rooms
                    .auth_chain
                    .get_auth_chain(room_id, state.iter().map(|(_, id)| id.clone()).collect())
                    .await?
                    .collect(),
            );
        }

        debug!("Loading fork states");

        let fork_states: Vec<_> = fork_states
            .into_iter()
            .map(|map| {
                map.into_iter()
                    .filter_map(|(k, id)| {
                        services()
                            .rooms
                            .short
                            .get_statekey_from_short(k)
                            .map(|(ty, st_key)| ((ty.to_string().into(), st_key), id))
                            .ok()
                    })
                    .collect::<StateMap<_>>()
            })
            .collect();

        debug!("Resolving state");

        let fetch_event = |id: &_| {
            let res = services().rooms.timeline.get_pdu(id);
            if let Err(e) = &res {
                error!("LOOK AT ME Failed to fetch event: {}", e);
            }
            res.ok().flatten()
        };

        let lock = services().globals.stateres_mutex.lock();
        let state = match state_res::resolve(auth_rules, &fork_states, auth_chain_sets, fetch_event)
        {
            Ok(new_state) => new_state,
            Err(_) => {
                return Err(Error::bad_database("State resolution failed, either an event could not be found or deserialization"));
            }
        };

        drop(lock);

        debug!("State resolution done. Compressing state");

        let new_room_state = state
            .into_iter()
            .map(|((event_type, state_key), event_id)| {
                let shortstatekey = services()
                    .rooms
                    .short
                    .get_or_create_shortstatekey(&event_type.to_string().into(), &state_key)?;
                services()
                    .rooms
                    .state_compressor
                    .compress_state_event(shortstatekey, &event_id)
            })
            .collect::<Result<_>>()?;

        Ok(Arc::new(new_room_state))
    }

    /// Find the event and auth it. Once the event is validated (steps 1 - 8)
    /// it is appended to the outliers Tree.
    ///
    /// Returns pdu and if we fetched it over federation the raw json.
    ///
    /// a. Look in the main timeline (pduid_pdu tree)
    /// b. Look at outlier pdu tree
    /// c. Ask origin server over federation
    /// d. TODO: Ask other servers over federation?
    #[allow(clippy::type_complexity)]
    #[tracing::instrument(skip_all)]
    pub(crate) fn fetch_and_handle_outliers<'a>(
        &'a self,
        origin: &'a ServerName,
        events: &'a [Arc<EventId>],
        create_event: &'a PduEvent,
        room_id: &'a RoomId,
        room_version_rules: &'a RoomVersionRules,
        pub_key_map: &'a RwLock<BTreeMap<String, SigningKeys>>,
    ) -> AsyncRecursiveType<'a, Vec<(Arc<PduEvent>, Option<BTreeMap<String, CanonicalJsonValue>>)>>
    {
        Box::pin(async move {
            let back_off = |id| async move {
                match services()
                    .globals
                    .bad_event_ratelimiter
                    .write()
                    .await
                    .entry(id)
                {
                    hash_map::Entry::Vacant(e) => {
                        e.insert((Instant::now(), 1));
                    }
                    hash_map::Entry::Occupied(mut e) => {
                        *e.get_mut() = (Instant::now(), e.get().1 + 1)
                    }
                }
            };

            let mut pdus = vec![];
            for id in events {
                // a. Look in the main timeline (pduid_pdu tree)
                // b. Look at outlier pdu tree
                // (get_pdu_json checks both)
                if let Ok(Some(local_pdu)) = services().rooms.timeline.get_pdu(id) {
                    trace!("Found {} in db", id);
                    pdus.push((local_pdu, None));
                    continue;
                }

                // c. Ask origin server over federation
                // We also handle its auth chain here so we don't get a stack overflow in
                // handle_outlier_pdu.
                let mut todo_auth_events = vec![Arc::clone(id)];
                let mut events_in_reverse_order = Vec::new();
                let mut events_all = HashSet::new();
                let mut i = 0;
                while let Some(next_id) = todo_auth_events.pop() {
                    if let Some((time, tries)) = services()
                        .globals
                        .bad_event_ratelimiter
                        .read()
                        .await
                        .get(&*next_id)
                    {
                        // Exponential backoff
                        let mut min_elapsed_duration =
                            Duration::from_secs(5 * 60) * (*tries) * (*tries);
                        if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
                            min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
                        }

                        if time.elapsed() < min_elapsed_duration {
                            info!("Backing off from {}", next_id);
                            continue;
                        }
                    }

                    if events_all.contains(&next_id) {
                        continue;
                    }

                    i += 1;
                    if i % 100 == 0 {
                        tokio::task::yield_now().await;
                    }

                    if let Ok(Some(_)) = services().rooms.timeline.get_pdu(&next_id) {
                        trace!("Found {} in db", next_id);
                        continue;
                    }

                    info!("Fetching {} over federation.", next_id);
                    match services()
                        .sending
                        .send_federation_request(
                            origin,
                            get_event::v1::Request {
                                event_id: (*next_id).to_owned(),
                            },
                        )
                        .await
                    {
                        Ok(res) => {
                            info!("Got {} over federation", next_id);
                            let (calculated_event_id, value) =
                                match pdu::gen_event_id_canonical_json(&res.pdu, room_version_rules)
                                {
                                    Ok(t) => t,
                                    Err(_) => {
                                        back_off((*next_id).to_owned()).await;
                                        continue;
                                    }
                                };

                            if calculated_event_id != *next_id {
                                warn!("Server didn't return event id we requested: requested: {}, we got {}. Event: {:?}",
                                    next_id, calculated_event_id, &res.pdu);
                            }

                            if let Some(auth_events) =
                                value.get("auth_events").and_then(|c| c.as_array())
                            {
                                for auth_event in auth_events {
                                    if let Ok(auth_event) =
                                        serde_json::from_value(auth_event.clone().into())
                                    {
                                        let a: Arc<EventId> = auth_event;
                                        todo_auth_events.push(a);
                                    } else {
                                        warn!("Auth event id is not valid");
                                    }
                                }
                            } else {
                                warn!("Auth event list invalid");
                            }

                            events_in_reverse_order.push((next_id.clone(), value));
                            events_all.insert(next_id);
                        }
                        Err(_) => {
                            warn!("Failed to fetch event: {}", next_id);
                            back_off((*next_id).to_owned()).await;
                        }
                    }
                }

                for (next_id, value) in events_in_reverse_order.iter().rev() {
                    if let Some((time, tries)) = services()
                        .globals
                        .bad_event_ratelimiter
                        .read()
                        .await
                        .get(&**next_id)
                    {
                        // Exponential backoff
                        let mut min_elapsed_duration =
                            Duration::from_secs(5 * 60) * (*tries) * (*tries);
                        if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
                            min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
                        }

                        if time.elapsed() < min_elapsed_duration {
                            info!("Backing off from {}", next_id);
                            continue;
                        }
                    }

                    match self
                        .handle_outlier_pdu(
                            origin,
                            create_event,
                            next_id,
                            room_id,
                            value.clone(),
                            true,
                            pub_key_map,
                        )
                        .await
                    {
                        Ok((pdu, json)) => {
                            if next_id == id {
                                pdus.push((pdu, Some(json)));
                            }
                        }
                        Err(e) => {
                            warn!("Authentication of event {} failed: {:?}", next_id, e);
                            back_off((**next_id).to_owned()).await;
                        }
                    }
                }
            }
            pdus
        })
    }

    async fn fetch_unknown_prev_events(
        &self,
        origin: &ServerName,
        create_event: &PduEvent,
        room_id: &RoomId,
        room_version_rules: &RoomVersionRules,
        pub_key_map: &RwLock<BTreeMap<String, SigningKeys>>,
        initial_set: Vec<Arc<EventId>>,
    ) -> Result<(
        Vec<Arc<EventId>>,
        HashMap<Arc<EventId>, (Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>)>,
    )> {
        let mut graph: HashMap<Arc<EventId>, _> = HashMap::new();
        let mut eventid_info = HashMap::new();
        let mut todo_outlier_stack: Vec<Arc<EventId>> = initial_set;

        let first_pdu_in_room = services()
            .rooms
            .timeline
            .first_pdu_in_room(room_id)?
            .ok_or_else(|| Error::bad_database("Failed to find first pdu in db."))?;

        let mut amount = 0;

        while let Some(prev_event_id) = todo_outlier_stack.pop() {
            if let Some((pdu, json_opt)) = self
                .fetch_and_handle_outliers(
                    origin,
                    &[prev_event_id.clone()],
                    create_event,
                    room_id,
                    room_version_rules,
                    pub_key_map,
                )
                .await
                .pop()
            {
                self.check_room_id(room_id, &pdu)?;

                if amount > services().globals.max_fetch_prev_events() {
                    // Max limit reached
                    warn!("Max prev event limit reached!");
                    graph.insert(prev_event_id.clone(), HashSet::new());
                    continue;
                }

                if let Some(json) = json_opt.or_else(|| {
                    services()
                        .rooms
                        .outlier
                        .get_outlier_pdu_json(&prev_event_id)
                        .ok()
                        .flatten()
                }) {
                    if pdu.origin_server_ts > first_pdu_in_room.origin_server_ts {
                        amount += 1;
                        for prev_prev in &pdu.prev_events {
                            if !graph.contains_key(prev_prev) {
                                todo_outlier_stack.push(prev_prev.clone());
                            }
                        }

                        graph.insert(
                            prev_event_id.clone(),
                            pdu.prev_events.iter().cloned().collect(),
                        );
                    } else {
                        // Time based check failed
                        graph.insert(prev_event_id.clone(), HashSet::new());
                    }

                    eventid_info.insert(prev_event_id.clone(), (pdu, json));
                } else {
                    // Get json failed, so this was not fetched over federation
                    graph.insert(prev_event_id.clone(), HashSet::new());
                }
            } else {
                // Fetch and handle failed
                graph.insert(prev_event_id.clone(), HashSet::new());
            }
        }

        let sorted = state_res::reverse_topological_power_sort(&graph, |event_id| {
            // This return value is the key used for sorting events,
            // events are then sorted by power level, time,
            // and lexically by event_id.
            Ok((
                int!(0).into(),
                MilliSecondsSinceUnixEpoch(
                    eventid_info
                        .get(event_id)
                        .map_or_else(|| uint!(0), |info| info.0.origin_server_ts),
                ),
            ))
        })
        .map_err(|_| Error::bad_database("Error sorting prev events"))?;

        Ok((sorted, eventid_info))
    }

    #[tracing::instrument(skip_all)]
    pub(crate) async fn fetch_required_signing_keys(
        &self,
        event: &BTreeMap<String, CanonicalJsonValue>,
        pub_key_map: &RwLock<BTreeMap<String, SigningKeys>>,
    ) -> Result<()> {
        let signatures = event
            .get("signatures")
            .ok_or(Error::BadServerResponse(
                "No signatures in server response pdu.",
            ))?
            .as_object()
            .ok_or(Error::BadServerResponse(
                "Invalid signatures object in server response pdu.",
            ))?;

        // We go through all the signatures we see on the value and fetch the corresponding signing
        // keys
        for (signature_server, signature) in signatures {
            let signature_object = signature.as_object().ok_or(Error::BadServerResponse(
                "Invalid signatures content object in server response pdu.",
            ))?;

            let signature_ids = signature_object.keys().cloned().collect::<Vec<_>>();

            let fetch_res = self
                .fetch_signing_keys(
                    signature_server.as_str().try_into().map_err(|_| {
                        Error::BadServerResponse(
                            "Invalid servername in signatures of server response pdu.",
                        )
                    })?,
                    signature_ids,
                    true,
                )
                .await;

            let keys = match fetch_res {
                Ok(keys) => keys,
                Err(_) => {
                    warn!("Signature verification failed: Could not fetch signing key.",);
                    continue;
                }
            };

            pub_key_map
                .write()
                .await
                .insert(signature_server.clone(), keys);
        }

        Ok(())
    }

    // Gets a list of servers for which we don't have the signing key yet. We go over
    // the PDUs and either cache the key or add it to the list that needs to be retrieved.
    async fn get_server_keys_from_cache(
        &self,
        pdu: &RawJsonValue,
        servers: &mut BTreeMap<OwnedServerName, BTreeMap<OwnedServerSigningKeyId, QueryCriteria>>,
        room_version_rules: &RoomVersionRules,
        pub_key_map: &mut RwLockWriteGuard<'_, BTreeMap<String, SigningKeys>>,
    ) -> Result<()> {
        let value: CanonicalJsonObject = serde_json::from_str(pdu.get()).map_err(|e| {
            error!("Invalid PDU in server response: {:?}: {:?}", pdu, e);
            Error::BadServerResponse("Invalid PDU in server response")
        })?;

        let event_id = format!(
            "${}",
            ruma::signatures::reference_hash(&value, room_version_rules)
                .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid PDU format"))?
        );
        let event_id = <&EventId>::try_from(event_id.as_str())
            .expect("ruma's reference hashes are valid event ids");

        if let Some((time, tries)) = services()
            .globals
            .bad_event_ratelimiter
            .read()
            .await
            .get(event_id)
        {
            // Exponential backoff
            let mut min_elapsed_duration = Duration::from_secs(30) * (*tries) * (*tries);
            if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
                min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
            }

            if time.elapsed() < min_elapsed_duration {
                debug!("Backing off from {}", event_id);
                return Err(Error::BadServerResponse("bad event, still backing off"));
            }
        }

        let origin_server_ts = value.get("origin_server_ts").ok_or_else(|| {
            error!("Invalid PDU, no origin_server_ts field");
            Error::BadRequest(
                ErrorKind::MissingParam,
                "Invalid PDU, no origin_server_ts field",
            )
        })?;

        let origin_server_ts: MilliSecondsSinceUnixEpoch = {
            let ts = origin_server_ts.as_integer().ok_or_else(|| {
                Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "origin_server_ts must be an integer",
                )
            })?;

            MilliSecondsSinceUnixEpoch(i64::from(ts).try_into().map_err(|_| {
                Error::BadRequest(ErrorKind::InvalidParam, "Time must be after the unix epoch")
            })?)
        };

        let signatures = value
            .get("signatures")
            .ok_or(Error::BadServerResponse(
                "No signatures in server response pdu.",
            ))?
            .as_object()
            .ok_or(Error::BadServerResponse(
                "Invalid signatures object in server response pdu.",
            ))?;

        for (signature_server, signature) in signatures {
            let signature_object = signature.as_object().ok_or(Error::BadServerResponse(
                "Invalid signatures content object in server response pdu.",
            ))?;

            let signature_ids = signature_object.keys().cloned().collect::<Vec<_>>();

            let contains_all_ids = |keys: &SigningKeys| {
                signature_ids.iter().all(|id| {
                    (keys.valid_until_ts > origin_server_ts
                        && keys
                            .verify_keys
                            .keys()
                            .map(ToString::to_string)
                            .any(|key_id| id == &key_id))
                        || keys
                            .old_verify_keys
                            .iter()
                            .any(|(key_id, key)| key_id == id && key.expired_ts > origin_server_ts)
                })
            };

            let origin = <&ServerName>::try_from(signature_server.as_str()).map_err(|_| {
                Error::BadServerResponse("Invalid servername in signatures of server response pdu.")
            })?;

            if servers.contains_key(origin) || pub_key_map.contains_key(origin.as_str()) {
                continue;
            }

            trace!("Loading signing keys for {}", origin);

            if let Some(result) = services().globals.signing_keys_for(origin)? {
                if !contains_all_ids(&result) {
                    trace!("Signing key not loaded for {}", origin);
                    servers.insert(origin.to_owned(), BTreeMap::new());
                }

                pub_key_map.insert(origin.to_string(), result);
            } else {
                servers.insert(origin.to_owned(), BTreeMap::new());
            }
        }

        Ok(())
    }

    pub(crate) async fn fetch_join_signing_keys(
        &self,
        event: &create_join_event::v2::Response,
        room_version_rules: &RoomVersionRules,
        pub_key_map: &RwLock<BTreeMap<String, SigningKeys>>,
    ) -> Result<()> {
        let mut servers: BTreeMap<
            OwnedServerName,
            BTreeMap<OwnedServerSigningKeyId, QueryCriteria>,
        > = BTreeMap::new();

        {
            let mut pkm = pub_key_map.write().await;

            // Try to fetch keys, failure is okay
            // Servers we couldn't find in the cache will be added to `servers`
            for pdu in &event.room_state.state {
                let _ = self
                    .get_server_keys_from_cache(pdu, &mut servers, room_version_rules, &mut pkm)
                    .await;
            }
            for pdu in &event.room_state.auth_chain {
                let _ = self
                    .get_server_keys_from_cache(pdu, &mut servers, room_version_rules, &mut pkm)
                    .await;
            }

            drop(pkm);
        }

        if servers.is_empty() {
            info!("We had all keys locally");
            return Ok(());
        }

        for server in services().globals.trusted_servers() {
            info!("Asking batch signing keys from trusted server {}", server);
            if let Ok(keys) = services()
                .sending
                .send_federation_request(
                    server,
                    get_remote_server_keys_batch::v2::Request {
                        server_keys: servers.clone(),
                    },
                )
                .await
            {
                trace!("Got signing keys: {:?}", keys);
                let mut pkm = pub_key_map.write().await;
                for k in keys.server_keys {
                    let k = match k.deserialize() {
                        Ok(key) => key,
                        Err(e) => {
                            warn!(
                                "Received error {} while fetching keys from trusted server {}",
                                e, server
                            );
                            warn!("{}", k.into_json());
                            continue;
                        }
                    };

                    // TODO: Check signature from trusted server?
                    servers.remove(&k.server_name);

                    let result = services()
                        .globals
                        .add_signing_key_from_trusted_server(&k.server_name, k.clone())?;

                    pkm.insert(k.server_name.to_string(), result);
                }
            }

            if servers.is_empty() {
                info!("Trusted server supplied all signing keys");
                return Ok(());
            }
        }

        info!("Asking individual servers for signing keys: {servers:?}");
        let mut futures: FuturesUnordered<_> = servers
            .into_keys()
            .map(|server| async move {
                (
                    services()
                        .sending
                        .send_federation_request(&server, get_server_keys::v2::Request::new())
                        .await,
                    server,
                )
            })
            .collect();

        while let Some(result) = futures.next().await {
            info!("Received new result");
            if let (Ok(get_keys_response), origin) = result {
                info!("Result is from {origin}");
                if let Ok(key) = get_keys_response.server_key.deserialize() {
                    let result = services()
                        .globals
                        .add_signing_key_from_origin(&origin, key)?;
                    pub_key_map.write().await.insert(origin.to_string(), result);
                }
            }
            info!("Done handling result");
        }

        info!("Search for signing keys done");

        Ok(())
    }

    /// Returns Ok if the acl allows the server
    pub fn acl_check(&self, server_name: &ServerName, room_id: &RoomId) -> Result<()> {
        let acl_event = match services().rooms.state_accessor.room_state_get(
            room_id,
            &StateEventType::RoomServerAcl,
            "",
        )? {
            Some(acl) => acl,
            None => return Ok(()),
        };

        let acl_event_content: RoomServerAclEventContent =
            match serde_json::from_str(acl_event.content.get()) {
                Ok(content) => content,
                Err(_) => {
                    warn!("Invalid ACL event");
                    return Ok(());
                }
            };

        if acl_event_content.is_allowed(server_name) {
            Ok(())
        } else {
            info!(
                "Server {} was denied by room ACL in {}",
                server_name, room_id
            );
            Err(Error::BadRequest(
                ErrorKind::forbidden(),
                "Server was denied by room ACL",
            ))
        }
    }

    /// Search the DB for the signing keys of the given server, if we don't have them
    /// fetch them from the server and save to our DB.
    #[tracing::instrument(skip_all)]
    pub async fn fetch_signing_keys(
        &self,
        origin: &ServerName,
        signature_ids: Vec<String>,
        // Whether to ask for keys from trusted servers. Should be false when getting
        // keys for validating requests, as per MSC4029
        query_via_trusted_servers: bool,
    ) -> Result<SigningKeys> {
        let contains_all_ids = |keys: &SigningKeys| {
            signature_ids.iter().all(|id| {
                keys.verify_keys
                    .keys()
                    .map(ToString::to_string)
                    .any(|key_id| id == &key_id)
                    || keys
                        .old_verify_keys
                        .keys()
                        .map(ToString::to_string)
                        .any(|key_id| id == &key_id)
            })
        };

        let permit = services()
            .globals
            .servername_ratelimiter
            .read()
            .await
            .get(origin)
            .map(|s| Arc::clone(s).acquire_owned());

        let permit = match permit {
            Some(p) => p,
            None => {
                let mut write = services().globals.servername_ratelimiter.write().await;
                let s = Arc::clone(
                    write
                        .entry(origin.to_owned())
                        .or_insert_with(|| Arc::new(Semaphore::new(1))),
                );

                s.acquire_owned()
            }
        }
        .await;

        let back_off = |id| async {
            match services()
                .globals
                .bad_signature_ratelimiter
                .write()
                .await
                .entry(id)
            {
                hash_map::Entry::Vacant(e) => {
                    e.insert((Instant::now(), 1));
                }
                hash_map::Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
            }
        };

        if let Some((time, tries)) = services()
            .globals
            .bad_signature_ratelimiter
            .read()
            .await
            .get(&signature_ids)
        {
            // Exponential backoff
            let mut min_elapsed_duration = Duration::from_secs(30) * (*tries) * (*tries);
            if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
                min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
            }

            if time.elapsed() < min_elapsed_duration {
                debug!("Backing off from {:?}", signature_ids);
                return Err(Error::BadServerResponse("bad signature, still backing off"));
            }
        }

        trace!("Loading signing keys for {}", origin);

        let result = services().globals.signing_keys_for(origin)?;

        let mut expires_soon_or_has_expired = false;

        if let Some(result) = result.clone() {
            let ts_threshold = MilliSecondsSinceUnixEpoch::from_system_time(
                SystemTime::now() + Duration::from_secs(30 * 60),
            )
            .expect("Should be valid until year 500,000,000");

            debug!(
                "The threshold is {:?}, found time is {:?} for server {}",
                ts_threshold, result.valid_until_ts, origin
            );

            if contains_all_ids(&result) {
                // We want to ensure that the keys remain valid by the time the other functions that handle signatures reach them
                if result.valid_until_ts > ts_threshold {
                    debug!(
                        "Keys for {} are deemed as valid, as they expire at {:?}",
                        &origin, &result.valid_until_ts
                    );
                    return Ok(result);
                }

                expires_soon_or_has_expired = true;
            }
        }

        let mut keys = result.unwrap_or_else(|| SigningKeys {
            verify_keys: BTreeMap::new(),
            old_verify_keys: BTreeMap::new(),
            valid_until_ts: MilliSecondsSinceUnixEpoch::now(),
        });

        // We want to set this to the max, and then lower it whenever we see older keys
        keys.valid_until_ts = MilliSecondsSinceUnixEpoch::from_system_time(
            SystemTime::now() + Duration::from_secs(7 * 86400),
        )
        .expect("Should be valid until year 500,000,000");

        debug!("Fetching signing keys for {} over federation", origin);

        if let Some(mut server_key) = services()
            .sending
            .send_federation_request(origin, get_server_keys::v2::Request::new())
            .await
            .ok()
            .and_then(|resp| resp.server_key.deserialize().ok())
        {
            // Keys should only be valid for a maximum of seven days
            server_key.valid_until_ts = server_key.valid_until_ts.min(
                MilliSecondsSinceUnixEpoch::from_system_time(
                    SystemTime::now() + Duration::from_secs(7 * 86400),
                )
                .expect("Should be valid until year 500,000,000"),
            );

            services()
                .globals
                .add_signing_key_from_origin(origin, server_key.clone())?;

            if keys.valid_until_ts > server_key.valid_until_ts {
                keys.valid_until_ts = server_key.valid_until_ts;
            }

            keys.verify_keys.extend(
                server_key
                    .verify_keys
                    .into_iter()
                    .map(|(id, key)| (id.to_string(), key)),
            );
            keys.old_verify_keys.extend(
                server_key
                    .old_verify_keys
                    .into_iter()
                    .map(|(id, key)| (id.to_string(), key)),
            );

            if contains_all_ids(&keys) {
                return Ok(keys);
            }
        }

        if query_via_trusted_servers {
            for server in services().globals.trusted_servers() {
                debug!("Asking {} for {}'s signing key", server, origin);
                if let Some(server_keys) = services()
                    .sending
                    .send_federation_request(
                        server,
                        get_remote_server_keys::v2::Request::new(
                            origin.to_owned(),
                            MilliSecondsSinceUnixEpoch::from_system_time(
                                SystemTime::now()
                                    .checked_add(Duration::from_secs(3600))
                                    .expect("SystemTime to large"),
                            )
                            .expect("time is valid"),
                        ),
                    )
                    .await
                    .ok()
                    .map(|resp| {
                        resp.server_keys
                            .into_iter()
                            .filter_map(|e| e.deserialize().ok())
                            .collect::<Vec<_>>()
                    })
                {
                    trace!("Got signing keys: {:?}", server_keys);
                    for mut k in server_keys {
                        if k.valid_until_ts
                        // Half an hour should give plenty of time for the server to respond with keys that are still
                        // valid, given we requested keys which are valid at least an hour from now
                            < MilliSecondsSinceUnixEpoch::from_system_time(
                                SystemTime::now() + Duration::from_secs(30 * 60),
                            )
                            .expect("Should be valid until year 500,000,000")
                        {
                            // Keys should only be valid for a maximum of seven days
                            k.valid_until_ts = k.valid_until_ts.min(
                                MilliSecondsSinceUnixEpoch::from_system_time(
                                    SystemTime::now() + Duration::from_secs(7 * 86400),
                                )
                                .expect("Should be valid until year 500,000,000"),
                            );

                            if keys.valid_until_ts > k.valid_until_ts {
                                keys.valid_until_ts = k.valid_until_ts;
                            }

                            services()
                                .globals
                                .add_signing_key_from_trusted_server(origin, k.clone())?;
                            keys.verify_keys.extend(
                                k.verify_keys
                                    .into_iter()
                                    .map(|(id, key)| (id.to_string(), key)),
                            );
                            keys.old_verify_keys.extend(
                                k.old_verify_keys
                                    .into_iter()
                                    .map(|(id, key)| (id.to_string(), key)),
                            );
                        } else {
                            warn!(
                                "Server {} gave us keys older than we requested, valid until: {:?}",
                                origin, k.valid_until_ts
                            );
                        }

                        if contains_all_ids(&keys) {
                            return Ok(keys);
                        }
                    }
                }
            }
        }

        // We should return these keys if fresher keys were not found
        if expires_soon_or_has_expired {
            info!("Returning stale keys for {}", origin);
            return Ok(keys);
        }

        drop(permit);

        back_off(signature_ids).await;

        warn!("Failed to find public key for server: {}", origin);
        Err(Error::BadServerResponse(
            "Failed to find public key for server",
        ))
    }

    fn check_room_id(&self, room_id: &RoomId, pdu: &PduEvent) -> Result<()> {
        if pdu.room_id != room_id {
            warn!("Found event from room {} in room {}", pdu.room_id, room_id);
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event has wrong room id",
            ));
        }
        Ok(())
    }
}
