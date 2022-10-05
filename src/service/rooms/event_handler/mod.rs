/// An async function that can recursively call itself.
type AsyncRecursiveType<'a, T> = Pin<Box<dyn Future<Output = T> + 'a + Send>>;

use ruma::{
    api::federation::discovery::{get_remote_server_keys, get_server_keys},
    signatures::CanonicalJsonObject,
    RoomVersionId,
};
use std::{
    collections::{btree_map, hash_map, BTreeMap, HashMap, HashSet},
    pin::Pin,
    sync::{Arc, RwLock, RwLockWriteGuard},
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::Semaphore;

use futures_util::{stream::FuturesUnordered, Future, StreamExt};
use ruma::{
    api::{
        client::error::ErrorKind,
        federation::{
            discovery::get_remote_server_keys_batch::{self, v2::QueryCriteria},
            event::{get_event, get_room_state_ids},
            membership::create_join_event,
        },
    },
    events::{
        room::{create::RoomCreateEventContent, server_acl::RoomServerAclEventContent},
        StateEventType,
    },
    int,
    serde::Base64,
    signatures::CanonicalJsonValue,
    state_res::{self, RoomVersion, StateMap},
    uint, EventId, MilliSecondsSinceUnixEpoch, RoomId, ServerName, ServerSigningKeyId,
};
use serde_json::value::{RawValue as RawJsonValue};
use tracing::{debug, error, info, trace, warn};

use crate::{service::*, services, Error, PduEvent, Result};

pub struct Service;

impl Service {
    /// When receiving an event one needs to:
    /// 0. Check the server is in the room
    /// 1. Skip the PDU if we already know about it
    /// 2. Check signatures, otherwise drop
    /// 3. Check content hash, redact if doesn't match
    /// 4. Fetch any missing auth events doing all checks listed here starting at 1. These are not
    ///    timeline events
    /// 5. Reject "due to auth events" if can't get all the auth events or some of the auth events are
    ///    also rejected "due to auth events"
    /// 6. Reject "due to auth events" if the event doesn't pass auth based on the auth events
    /// 7. Persist this event as an outlier
    /// 8. If not timeline event: stop
    /// 9. Fetch any missing prev events doing all checks listed here starting at 1. These are timeline
    ///    events
    /// 10. Fetch missing state and auth chain events by calling /state_ids at backwards extremities
    ///     doing all the checks in this list starting at 1. These are not timeline events
    /// 11. Check the auth of the event passes based on the state of the event
    /// 12. Ensure that the state is derived from the previous current state (i.e. we calculated by
    ///     doing state res where one of the inputs was a previously trusted set of state, don't just
    ///     trust a set of state we got from a remote)
    /// 13. Check if the event passes auth based on the "current state" of the room, if not "soft fail"
    ///     it
    /// 14. Use state resolution to find new room state
    // We use some AsyncRecursiveType hacks here so we can call this async funtion recursively
    #[tracing::instrument(skip(self, value, is_timeline_event, pub_key_map))]
    pub(crate) async fn handle_incoming_pdu<'a>(
        &self,
        origin: &'a ServerName,
        event_id: &'a EventId,
        room_id: &'a RoomId,
        value: BTreeMap<String, CanonicalJsonValue>,
        is_timeline_event: bool,
        pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    ) -> Result<Option<Vec<u8>>> {
        if !services().rooms.metadata.exists(room_id)? {
            return Err(Error::BadRequest(
                ErrorKind::NotFound,
                "Room is unknown to this server",
            ));
        }

        if services().rooms.metadata.is_disabled(room_id)? {
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "Federation of this room is currently disabled on this server.",
            ));
        }

        // 1. Skip the PDU if we already have it as a timeline event
        if let Some(pdu_id) = services().rooms.timeline.get_pdu_id(event_id)? {
            return Ok(Some(pdu_id.to_vec()));
        }

        let create_event = services()
            .rooms
            .state_accessor
            .room_state_get(room_id, &StateEventType::RoomCreate, "")?
            .ok_or_else(|| Error::bad_database("Failed to find create event in db."))?;

        let first_pdu_in_room = services()
            .rooms
            .timeline
            .first_pdu_in_room(room_id)?
            .ok_or_else(|| Error::bad_database("Failed to find first pdu in db."))?;

        let (incoming_pdu, val) = self
            .handle_outlier_pdu(origin, &create_event, event_id, room_id, value, pub_key_map)
            .await?;

        // 8. if not timeline event: stop
        if !is_timeline_event {
            return Ok(None);
        }

        // Skip old events
        if incoming_pdu.origin_server_ts < first_pdu_in_room.origin_server_ts {
            return Ok(None);
        }

        // 9. Fetch any missing prev events doing all checks listed here starting at 1. These are timeline events
        let (sorted_prev_events, mut eventid_info) = self
            .fetch_unknown_prev_events(
                origin,
                &create_event,
                room_id,
                pub_key_map,
                incoming_pdu.prev_events.clone(),
            )
            .await?;

        let mut errors = 0;
        for prev_id in dbg!(sorted_prev_events) {
            // Check for disabled again because it might have changed
            if services().rooms.metadata.is_disabled(room_id)? {
                return Err(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "Federation of this room is currently disabled on this server.",
                ));
            }

            if let Some((time, tries)) = services()
                .globals
                .bad_event_ratelimiter
                .read()
                .unwrap()
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
                break;
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
                    .unwrap()
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
                        .unwrap()
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
                    .unwrap()
                    .remove(&room_id.to_owned());
                warn!(
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
            .unwrap()
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
            .unwrap()
            .remove(&room_id.to_owned());

        r
    }

    #[tracing::instrument(skip(self, create_event, value, pub_key_map))]
    fn handle_outlier_pdu<'a>(
        &self,
        origin: &'a ServerName,
        create_event: &'a PduEvent,
        event_id: &'a EventId,
        room_id: &'a RoomId,
        value: BTreeMap<String, CanonicalJsonValue>,
        pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    ) -> AsyncRecursiveType<'a, Result<(Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>)>> {
        Box::pin(async move {
            // TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json

            // We go through all the signatures we see on the value and fetch the corresponding signing
            // keys
            self.fetch_required_signing_keys(&value, pub_key_map)
                .await?;

            // 2. Check signatures, otherwise drop
            // 3. check content hash, redact if doesn't match
            let create_event_content: RoomCreateEventContent =
                serde_json::from_str(create_event.content.get()).map_err(|e| {
                    error!("Invalid create event: {}", e);
                    Error::BadDatabase("Invalid create event in db")
                })?;

            let room_version_id = &create_event_content.room_version;
            let room_version =
                RoomVersion::new(room_version_id).expect("room version is supported");

            let mut val = match ruma::signatures::verify_event(
                &*pub_key_map.read().expect("RwLock is poisoned."),
                &value,
                room_version_id,
            ) {
                Err(e) => {
                    // Drop
                    warn!("Dropping bad event {}: {}", event_id, e);
                    return Err(Error::BadRequest(
                        ErrorKind::InvalidParam,
                        "Signature verification failed",
                    ));
                }
                Ok(ruma::signatures::Verified::Signatures) => {
                    // Redact
                    warn!("Calculated hash does not match: {}", event_id);
                    match ruma::signatures::redact(&value, room_version_id) {
                        Ok(obj) => obj,
                        Err(_) => {
                            return Err(Error::BadRequest(
                                ErrorKind::InvalidParam,
                                "Redaction failed",
                            ))
                        }
                    }
                }
                Ok(ruma::signatures::Verified::All) => value,
            };

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

            // 4. fetch any missing auth events doing all checks listed here starting at 1. These are not timeline events
            // 5. Reject "due to auth events" if can't get all the auth events or some of the auth events are also rejected "due to auth events"
            // NOTE: Step 5 is not applied anymore because it failed too often
            warn!("Fetching auth events for {}", incoming_pdu.event_id);
            self.fetch_and_handle_outliers(
                origin,
                &incoming_pdu
                    .auth_events
                    .iter()
                    .map(|x| Arc::from(&**x))
                    .collect::<Vec<_>>(),
                create_event,
                room_id,
                pub_key_map,
            )
            .await;

            // 6. Reject "due to auth events" if the event doesn't pass auth based on the auth events
            info!(
                "Auth check for {} based on auth events",
                incoming_pdu.event_id
            );

            // Build map of auth events
            let mut auth_events = HashMap::new();
            for id in &incoming_pdu.auth_events {
                let auth_event = match services().rooms.timeline.get_pdu(id)? {
                    Some(e) => e,
                    None => {
                        warn!("Could not find auth event {}", id);
                        continue;
                    }
                };

                match auth_events.entry((
                    auth_event.kind.to_string().into(),
                    auth_event
                        .state_key
                        .clone()
                        .expect("all auth events have state keys"),
                )) {
                    hash_map::Entry::Vacant(v) => {
                        v.insert(auth_event);
                    }
                    hash_map::Entry::Occupied(_) => {
                        return Err(Error::BadRequest(
                            ErrorKind::InvalidParam,
                            "Auth event's type and state_key combination exists multiple times.",
                        ));
                    }
                }
            }

            // The original create event must be in the auth events
            if auth_events
                .get(&(StateEventType::RoomCreate, "".to_owned()))
                .map(|a| a.as_ref())
                != Some(create_event)
            {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Incoming event refers to wrong create event.",
                ));
            }

            if !state_res::event_auth::auth_check(
                &room_version,
                &incoming_pdu,
                None::<PduEvent>, // TODO: third party invite
                |k, s| auth_events.get(&(k.to_string().into(), s.to_owned())),
            )
            .map_err(|_e| Error::BadRequest(ErrorKind::InvalidParam, "Auth check failed"))?
            {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Auth check failed",
                ));
            }

            info!("Validation successful.");

            // 7. Persist the event as an outlier.
            services()
                .rooms
                .outlier
                .add_pdu_outlier(&incoming_pdu.event_id, &val)?;

            info!("Added pdu as outlier.");

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
        pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
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
        let room_version = RoomVersion::new(room_version_id).expect("room version is supported");

        // 10. Fetch missing state and auth chain events by calling /state_ids at backwards extremities
        //     doing all the checks in this list starting at 1. These are not timeline events.

        // TODO: if we know the prev_events of the incoming event we can avoid the request and build
        // the state from a known point and resolve if > 1 prev_event

        info!("Requesting state at event");
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
                info!("Using cached state");
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
            info!("Calculating state at event using state res");
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
                    let mut leaf_state: BTreeMap<_, _> = services()
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

                let result =
                    state_res::resolve(room_version_id, &fork_states, auth_chain_sets, |id| {
                        let res = services().rooms.timeline.get_pdu(id);
                        if let Err(e) = &res {
                            error!("LOOK AT ME Failed to fetch event: {}", e);
                        }
                        res.ok().flatten()
                    });
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
            info!("Calling /state_ids");
            // Call /state_ids to find out what the state at this pdu is. We trust the server's
            // response to some extend, but we still do a lot of checks on the events
            match services()
                .sending
                .send_federation_request(
                    origin,
                    get_room_state_ids::v1::Request {
                        room_id,
                        event_id: &incoming_pdu.event_id,
                    },
                )
                .await
            {
                Ok(res) => {
                    info!("Fetching state events at event.");
                    let state_vec = self
                        .fetch_and_handle_outliers(
                            origin,
                            &res.pdu_ids
                                .iter()
                                .map(|x| Arc::from(&**x))
                                .collect::<Vec<_>>(),
                            create_event,
                            room_id,
                            pub_key_map,
                        )
                        .await;

                    let mut state: BTreeMap<_, Arc<EventId>> = BTreeMap::new();
                    for (pdu, _) in state_vec {
                        let state_key = pdu.state_key.clone().ok_or_else(|| {
                            Error::bad_database("Found non-state pdu in state events.")
                        })?;

                        let shortstatekey = services().rooms.short.get_or_create_shortstatekey(
                            &pdu.kind.to_string().into(),
                            &state_key,
                        )?;

                        match state.entry(shortstatekey) {
                            btree_map::Entry::Vacant(v) => {
                                v.insert(Arc::from(&*pdu.event_id));
                            }
                            btree_map::Entry::Occupied(_) => return Err(
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

        info!("Starting auth check");
        // 11. Check the auth of the event passes based on the state of the event
        let check_result = state_res::event_auth::auth_check(
            &room_version,
            &incoming_pdu,
            None::<PduEvent>, // TODO: third party invite
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
        .map_err(|_e| Error::BadRequest(ErrorKind::InvalidParam, "Auth check failed."))?;

        if !check_result {
            return Err(Error::bad_database(
                "Event has failed auth check with state at the event.",
            ));
        }
        info!("Auth check succeeded");

        // We start looking at current room state now, so lets lock the room

        let mutex_state = Arc::clone(
            services()
                .globals
                .roomid_mutex_state
                .write()
                .unwrap()
                .entry(room_id.to_owned())
                .or_default(),
        );
        let state_lock = mutex_state.lock().await;

        // Now we calculate the set of extremities this room has after the incoming event has been
        // applied. We start with the previous extremities (aka leaves)
        info!("Calculating extremities");
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

        info!("Compressing state at event");
        let state_ids_compressed = state_at_incoming_event
            .iter()
            .map(|(shortstatekey, id)| {
                services()
                    .rooms
                    .state_compressor
                    .compress_state_event(*shortstatekey, id)
            })
            .collect::<Result<_>>()?;

        // 13. Check if the event passes auth based on the "current state" of the room, if not "soft fail" it
        info!("Starting soft fail auth check");

        let auth_events = services().rooms.state.get_auth_events(
            room_id,
            &incoming_pdu.kind,
            &incoming_pdu.sender,
            incoming_pdu.state_key.as_deref(),
            &incoming_pdu.content,
        )?;

        let soft_fail = !state_res::event_auth::auth_check(
            &room_version,
            &incoming_pdu,
            None::<PduEvent>,
            |k, s| auth_events.get(&(k.clone(), s.to_owned())),
        )
        .map_err(|_e| Error::BadRequest(ErrorKind::InvalidParam, "Auth check failed."))?;

        if soft_fail {
            services().rooms.timeline.append_incoming_pdu(
                &incoming_pdu,
                val,
                extremities.iter().map(|e| (**e).to_owned()).collect(),
                state_ids_compressed,
                soft_fail,
                &state_lock,
            )?;

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

        if incoming_pdu.state_key.is_some() {
            info!("Loading current room state ids");
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

            info!("Preparing for stateres to derive new room state");
            let mut extremity_sstatehashes = HashMap::new();

            info!("Loading extremities");
            for id in dbg!(&extremities) {
                match services().rooms.timeline.get_pdu(id)? {
                    Some(leaf_pdu) => {
                        extremity_sstatehashes.insert(
                            services()
                                .rooms
                                .state_accessor
                                .pdu_shortstatehash(&leaf_pdu.event_id)?
                                .ok_or_else(|| {
                                    error!(
                                        "Found extremity pdu with no statehash in db: {:?}",
                                        leaf_pdu
                                    );
                                    Error::bad_database("Found pdu with no statehash in db.")
                                })?,
                            leaf_pdu,
                        );
                    }
                    _ => {
                        error!("Missing state snapshot for {:?}", id);
                        return Err(Error::BadDatabase("Missing state snapshot."));
                    }
                }
            }

            let mut fork_states = Vec::new();

            // 12. Ensure that the state is derived from the previous current state (i.e. we calculated
            //     by doing state res where one of the inputs was a previously trusted set of state,
            //     don't just trust a set of state we got from a remote).

            // We do this by adding the current state to the list of fork states
            extremity_sstatehashes.remove(&current_sstatehash);
            fork_states.push(current_state_ids);

            // We also add state after incoming event to the fork states
            let mut state_after = state_at_incoming_event.clone();
            if let Some(state_key) = &incoming_pdu.state_key {
                let shortstatekey = services().rooms.short.get_or_create_shortstatekey(
                    &incoming_pdu.kind.to_string().into(),
                    state_key,
                )?;

                state_after.insert(shortstatekey, Arc::from(&*incoming_pdu.event_id));
            }
            fork_states.push(state_after);

            let mut update_state = false;
            // 14. Use state resolution to find new room state
            let new_room_state = if fork_states.is_empty() {
                panic!("State is empty");
            } else if fork_states.iter().skip(1).all(|f| &fork_states[0] == f) {
                info!("State resolution trivial");
                // There was only one state, so it has to be the room's current state (because that is
                // always included)
                fork_states[0]
                    .iter()
                    .map(|(k, id)| {
                        services()
                            .rooms
                            .state_compressor
                            .compress_state_event(*k, id)
                    })
                    .collect::<Result<_>>()?
            } else {
                info!("Loading auth chains");
                // We do need to force an update to this room's state
                update_state = true;

                let mut auth_chain_sets = Vec::new();
                for state in &fork_states {
                    auth_chain_sets.push(
                        services()
                            .rooms
                            .auth_chain
                            .get_auth_chain(
                                room_id,
                                state.iter().map(|(_, id)| id.clone()).collect(),
                            )
                            .await?
                            .collect(),
                    );
                }

                info!("Loading fork states");

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

                info!("Resolving state");

                let lock = services().globals.stateres_mutex.lock();
                let state = match state_res::resolve(
                    room_version_id,
                    &fork_states,
                    auth_chain_sets,
                    |id| {
                        let res = services().rooms.timeline.get_pdu(id);
                        if let Err(e) = &res {
                            error!("LOOK AT ME Failed to fetch event: {}", e);
                        }
                        res.ok().flatten()
                    },
                ) {
                    Ok(new_state) => new_state,
                    Err(_) => {
                        return Err(Error::bad_database("State resolution failed, either an event could not be found or deserialization"));
                    }
                };

                drop(lock);

                info!("State resolution done. Compressing state");

                state
                    .into_iter()
                    .map(|((event_type, state_key), event_id)| {
                        let shortstatekey = services().rooms.short.get_or_create_shortstatekey(
                            &event_type.to_string().into(),
                            &state_key,
                        )?;
                        services()
                            .rooms
                            .state_compressor
                            .compress_state_event(shortstatekey, &event_id)
                    })
                    .collect::<Result<_>>()?
            };

            // Set the new room state to the resolved state
            if update_state {
                info!("Forcing new room state");
                let sstatehash = services()
                    .rooms
                    .state_compressor
                    .save_state(room_id, new_room_state)?;
                services()
                    .rooms
                    .state
                    .set_room_state(room_id, sstatehash, &state_lock)?;
            }
        }

        info!("Appending pdu to timeline");
        extremities.insert(incoming_pdu.event_id.clone());

        // Now that the event has passed all auth it is added into the timeline.
        // We use the `state_at_event` instead of `state_after` so we accurately
        // represent the state for this event.

        let pdu_id = services().rooms.timeline.append_incoming_pdu(
            &incoming_pdu,
            val,
            extremities.iter().map(|e| (**e).to_owned()).collect(),
            state_ids_compressed,
            soft_fail,
            &state_lock,
        )?;

        info!("Appended incoming pdu");

        // Event has passed all auth/stateres checks
        drop(state_lock);
        Ok(pdu_id)
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
    #[tracing::instrument(skip_all)]
    pub(crate) fn fetch_and_handle_outliers<'a>(
        &self,
        origin: &'a ServerName,
        events: &'a [Arc<EventId>],
        create_event: &'a PduEvent,
        room_id: &'a RoomId,
        pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    ) -> AsyncRecursiveType<'a, Vec<(Arc<PduEvent>, Option<BTreeMap<String, CanonicalJsonValue>>)>>
    {
        Box::pin(async move {
            let back_off = |id| match services()
                .globals
                .bad_event_ratelimiter
                .write()
                .unwrap()
                .entry(id)
            {
                hash_map::Entry::Vacant(e) => {
                    e.insert((Instant::now(), 1));
                }
                hash_map::Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
            };

            let mut pdus = vec![];
            for id in events {
                if let Some((time, tries)) = services()
                    .globals
                    .bad_event_ratelimiter
                    .read()
                    .unwrap()
                    .get(&**id)
                {
                    // Exponential backoff
                    let mut min_elapsed_duration =
                        Duration::from_secs(5 * 60) * (*tries) * (*tries);
                    if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
                        min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
                    }

                    if time.elapsed() < min_elapsed_duration {
                        info!("Backing off from {}", id);
                        continue;
                    }
                }

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
                    if events_all.contains(&next_id) {
                        continue;
                    }

                    i += 1;
                    if i % 100 == 0 {
                        tokio::task::yield_now().await;
                    }

                    if let Ok(Some(_)) = services().rooms.timeline.get_pdu(&next_id) {
                        trace!("Found {} in db", id);
                        continue;
                    }

                    info!("Fetching {} over federation.", next_id);
                    match services()
                        .sending
                        .send_federation_request(
                            origin,
                            get_event::v1::Request { event_id: &next_id },
                        )
                        .await
                    {
                        Ok(res) => {
                            info!("Got {} over federation", next_id);
                            let (calculated_event_id, value) =
                                match pdu::gen_event_id_canonical_json(&res.pdu) {
                                    Ok(t) => t,
                                    Err(_) => {
                                        back_off((*next_id).to_owned());
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
                            back_off((*next_id).to_owned());
                        }
                    }
                }

                for (next_id, value) in events_in_reverse_order.iter().rev() {
                    match self
                        .handle_outlier_pdu(
                            origin,
                            create_event,
                            next_id,
                            room_id,
                            value.clone(),
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
                            back_off((**next_id).to_owned());
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
        pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
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
                    &create_event,
                    room_id,
                    pub_key_map,
                )
                .await
                .pop()
            {
                if amount > 100 {
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
                                todo_outlier_stack.push(dbg!(prev_prev.clone()));
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

        let sorted = state_res::lexicographical_topological_sort(dbg!(&graph), |event_id| {
            // This return value is the key used for sorting events,
            // events are then sorted by power level, time,
            // and lexically by event_id.
            println!("{}", event_id);
            Ok((
                int!(0),
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
        pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
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
                .map_err(|_| Error::bad_database("RwLock is poisoned."))?
                .insert(signature_server.clone(), keys);
        }

        Ok(())
    }

    // Gets a list of servers for which we don't have the signing key yet. We go over
    // the PDUs and either cache the key or add it to the list that needs to be retrieved.
    fn get_server_keys_from_cache(
        &self,
        pdu: &RawJsonValue,
        servers: &mut BTreeMap<Box<ServerName>, BTreeMap<Box<ServerSigningKeyId>, QueryCriteria>>,
        room_version: &RoomVersionId,
        pub_key_map: &mut RwLockWriteGuard<'_, BTreeMap<String, BTreeMap<String, Base64>>>,
    ) -> Result<()> {
        let value: CanonicalJsonObject = serde_json::from_str(pdu.get()).map_err(|e| {
            error!("Invalid PDU in server response: {:?}: {:?}", pdu, e);
            Error::BadServerResponse("Invalid PDU in server response")
        })?;

        let event_id = format!(
            "${}",
            ruma::signatures::reference_hash(&value, room_version)
                .expect("ruma can calculate reference hashes")
        );
        let event_id = <&EventId>::try_from(event_id.as_str())
            .expect("ruma's reference hashes are valid event ids");

        if let Some((time, tries)) = services()
            .globals
            .bad_event_ratelimiter
            .read()
            .unwrap()
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

            let contains_all_ids = |keys: &BTreeMap<String, Base64>| {
                signature_ids.iter().all(|id| keys.contains_key(id))
            };

            let origin = <&ServerName>::try_from(signature_server.as_str()).map_err(|_| {
                Error::BadServerResponse("Invalid servername in signatures of server response pdu.")
            })?;

            if servers.contains_key(origin) || pub_key_map.contains_key(origin.as_str()) {
                continue;
            }

            trace!("Loading signing keys for {}", origin);

            let result: BTreeMap<_, _> = services()
                .globals
                .signing_keys_for(origin)?
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.key))
                .collect();

            if !contains_all_ids(&result) {
                trace!("Signing key not loaded for {}", origin);
                servers.insert(origin.to_owned(), BTreeMap::new());
            }

            pub_key_map.insert(origin.to_string(), result);
        }

        Ok(())
    }

    pub(crate) async fn fetch_join_signing_keys(
        &self,
        event: &create_join_event::v2::Response,
        room_version: &RoomVersionId,
        pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    ) -> Result<()> {
        let mut servers: BTreeMap<
            Box<ServerName>,
            BTreeMap<Box<ServerSigningKeyId>, QueryCriteria>,
        > = BTreeMap::new();

        {
            let mut pkm = pub_key_map
                .write()
                .map_err(|_| Error::bad_database("RwLock is poisoned."))?;

            // Try to fetch keys, failure is okay
            // Servers we couldn't find in the cache will be added to `servers`
            for pdu in &event.room_state.state {
                let _ = self.get_server_keys_from_cache(pdu, &mut servers, room_version, &mut pkm);
            }
            for pdu in &event.room_state.auth_chain {
                let _ = self.get_server_keys_from_cache(pdu, &mut servers, room_version, &mut pkm);
            }

            drop(pkm);
        }

        if servers.is_empty() {
            // We had all keys locally
            return Ok(());
        }

        for server in services().globals.trusted_servers() {
            trace!("Asking batch signing keys from trusted server {}", server);
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
                let mut pkm = pub_key_map
                    .write()
                    .map_err(|_| Error::bad_database("RwLock is poisoned."))?;
                for k in keys.server_keys {
                    let k = k.deserialize().unwrap();

                    // TODO: Check signature from trusted server?
                    servers.remove(&k.server_name);

                    let result = services()
                        .globals
                        .add_signing_key(&k.server_name, k.clone())?
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.key))
                        .collect::<BTreeMap<_, _>>();

                    pkm.insert(k.server_name.to_string(), result);
                }
            }

            if servers.is_empty() {
                return Ok(());
            }
        }

        let mut futures: FuturesUnordered<_> = servers
            .into_iter()
            .map(|(server, _)| async move {
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
            if let (Ok(get_keys_response), origin) = result {
                let result: BTreeMap<_, _> = services()
                    .globals
                    .add_signing_key(&origin, get_keys_response.server_key.deserialize().unwrap())?
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.key))
                    .collect();

                pub_key_map
                    .write()
                    .map_err(|_| Error::bad_database("RwLock is poisoned."))?
                    .insert(origin.to_string(), result);
            }
        }

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
            Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "Server was denied by ACL",
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
    ) -> Result<BTreeMap<String, Base64>> {
        let contains_all_ids =
            |keys: &BTreeMap<String, Base64>| signature_ids.iter().all(|id| keys.contains_key(id));

        let permit = services()
            .globals
            .servername_ratelimiter
            .read()
            .unwrap()
            .get(origin)
            .map(|s| Arc::clone(s).acquire_owned());

        let permit = match permit {
            Some(p) => p,
            None => {
                let mut write = services().globals.servername_ratelimiter.write().unwrap();
                let s = Arc::clone(
                    write
                        .entry(origin.to_owned())
                        .or_insert_with(|| Arc::new(Semaphore::new(1))),
                );

                s.acquire_owned()
            }
        }
        .await;

        let back_off = |id| match services()
            .globals
            .bad_signature_ratelimiter
            .write()
            .unwrap()
            .entry(id)
        {
            hash_map::Entry::Vacant(e) => {
                e.insert((Instant::now(), 1));
            }
            hash_map::Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
        };

        if let Some((time, tries)) = services()
            .globals
            .bad_signature_ratelimiter
            .read()
            .unwrap()
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

        let mut result: BTreeMap<_, _> = services()
            .globals
            .signing_keys_for(origin)?
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.key))
            .collect();

        if contains_all_ids(&result) {
            return Ok(result);
        }

        debug!("Fetching signing keys for {} over federation", origin);

        if let Some(server_key) = services()
            .sending
            .send_federation_request(origin, get_server_keys::v2::Request::new())
            .await
            .ok()
            .and_then(|resp| resp.server_key.deserialize().ok())
        {
            services()
                .globals
                .add_signing_key(origin, server_key.clone())?;

            result.extend(
                server_key
                    .verify_keys
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.key)),
            );
            result.extend(
                server_key
                    .old_verify_keys
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.key)),
            );

            if contains_all_ids(&result) {
                return Ok(result);
            }
        }

        for server in services().globals.trusted_servers() {
            debug!("Asking {} for {}'s signing key", server, origin);
            if let Some(server_keys) = services()
                .sending
                .send_federation_request(
                    server,
                    get_remote_server_keys::v2::Request::new(
                        origin,
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
                for k in server_keys {
                    services().globals.add_signing_key(origin, k.clone())?;
                    result.extend(
                        k.verify_keys
                            .into_iter()
                            .map(|(k, v)| (k.to_string(), v.key)),
                    );
                    result.extend(
                        k.old_verify_keys
                            .into_iter()
                            .map(|(k, v)| (k.to_string(), v.key)),
                    );
                }

                if contains_all_ids(&result) {
                    return Ok(result);
                }
            }
        }

        drop(permit);

        back_off(signature_ids);

        warn!("Failed to find public key for server: {}", origin);
        Err(Error::BadServerResponse(
            "Failed to find public key for server",
        ))
    }
}
