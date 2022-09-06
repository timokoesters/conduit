/// An async function that can recursively call itself.
type AsyncRecursiveType<'a, T> = Pin<Box<dyn Future<Output = T> + 'a + Send>>;

use std::{
    collections::{btree_map, hash_map, BTreeMap, HashMap, HashSet},
    pin::Pin,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};

use futures_util::Future;
use ruma::{
    api::{
        client::error::ErrorKind,
        federation::event::{get_event, get_room_state_ids},
    },
    events::{room::create::RoomCreateEventContent, StateEventType},
    int,
    serde::Base64,
    signatures::CanonicalJsonValue,
    state_res::{self, RoomVersion, StateMap},
    uint, EventId, MilliSecondsSinceUnixEpoch, RoomId, ServerName,
};
use tracing::{error, info, trace, warn};

use crate::{service::*, services, Error, PduEvent};

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
    #[tracing::instrument(skip(value, is_timeline_event, pub_key_map))]
    pub(crate) async fn handle_incoming_pdu<'a>(
        &self,
        origin: &'a ServerName,
        event_id: &'a EventId,
        room_id: &'a RoomId,
        value: BTreeMap<String, CanonicalJsonValue>,
        is_timeline_event: bool,
        pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    ) -> Result<Option<Vec<u8>>> {
        services().rooms.exists(room_id)?.ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Room is unknown to this server",
        ))?;

        services()
            .rooms
            .is_disabled(room_id)?
            .ok_or(Error::BadRequest(
                ErrorKind::Forbidden,
                "Federation of this room is currently disabled on this server.",
            ))?;

        // 1. Skip the PDU if we already have it as a timeline event
        if let Some(pdu_id) = services().rooms.get_pdu_id(event_id)? {
            return Ok(Some(pdu_id.to_vec()));
        }

        let create_event = services()
            .rooms
            .room_state_get(room_id, &StateEventType::RoomCreate, "")?
            .ok_or_else(|| Error::bad_database("Failed to find create event in db."))?;

        let first_pdu_in_room = services()
            .rooms
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
        let (sorted_prev_events, eventid_info) = self.fetch_unknown_prev_events(
            origin,
            &create_event,
            room_id,
            pub_key_map,
            incoming_pdu.prev_events.clone(),
        );

        let mut errors = 0;
        for prev_id in dbg!(sorted_prev_events) {
            // Check for disabled again because it might have changed
            services()
                .rooms
                .is_disabled(room_id)?
                .ok_or(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "Federation of
                    this room is currently disabled on this server.",
                ))?;

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
        let r = services().rooms.event_handler.upgrade_outlier_to_timeline_pdu(
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

    #[tracing::instrument(skip(create_event, value, pub_key_map))]
    fn handle_outlier_pdu<'a>(
        &self,
        origin: &'a ServerName,
        create_event: &'a PduEvent,
        event_id: &'a EventId,
        room_id: &'a RoomId,
        value: BTreeMap<String, CanonicalJsonValue>,
        pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    ) -> AsyncRecursiveType<'a, Result<(Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>), String>>
    {
        Box::pin(async move {
            // TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json

            // We go through all the signatures we see on the value and fetch the corresponding signing
            // keys
            self.fetch_required_signing_keys(&value, pub_key_map, db)
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
                &*pub_key_map.read().map_err(|_| "RwLock is poisoned.")?,
                &value,
                room_version_id,
            ) {
                Err(e) => {
                    // Drop
                    warn!("Dropping bad event {}: {}", event_id, e);
                    return Err("Signature verification failed".to_owned());
                }
                Ok(ruma::signatures::Verified::Signatures) => {
                    // Redact
                    warn!("Calculated hash does not match: {}", event_id);
                    match ruma::signatures::redact(&value, room_version_id) {
                        Ok(obj) => obj,
                        Err(_) => return Err("Redaction failed".to_owned()),
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
            .map_err(|_| "Event is not a valid PDU.".to_owned())?;

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
                let auth_event = match services().rooms.get_pdu(id)? {
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
            .map_err(|e| {
                error!(e);
                Error::BadRequest(ErrorKind::InvalidParam, "Auth check failed")
            })? {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Auth check failed",
                ));
            }

            info!("Validation successful.");

            // 7. Persist the event as an outlier.
            services()
                .rooms
                .add_pdu_outlier(&incoming_pdu.event_id, &val)?;

            info!("Added pdu as outlier.");

            Ok((Arc::new(incoming_pdu), val))
        })
    }

    #[tracing::instrument(skip(incoming_pdu, val, create_event, pub_key_map))]
    pub async fn upgrade_outlier_to_timeline_pdu(
        &self,
        incoming_pdu: Arc<PduEvent>,
        val: BTreeMap<String, CanonicalJsonValue>,
        create_event: &PduEvent,
        origin: &ServerName,
        room_id: &RoomId,
        pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    ) -> Result<Option<Vec<u8>>, String> {
        // Skip the PDU if we already have it as a timeline event
        if let Ok(Some(pduid)) = services().rooms.get_pdu_id(&incoming_pdu.event_id) {
            return Ok(Some(pduid));
        }

        if services()
            .rooms
            .is_event_soft_failed(&incoming_pdu.event_id)
            .map_err(|_| "Failed to ask db for soft fail".to_owned())?
        {
            return Err("Event has been soft failed".into());
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
                .pdu_shortstatehash(prev_event)
                .map_err(|_| "Failed talking to db".to_owned())?;

            let state = if let Some(shortstatehash) = prev_event_sstatehash {
                Some(services().rooms.state_full_ids(shortstatehash).await)
            } else {
                None
            };

            if let Some(Ok(mut state)) = state {
                info!("Using cached state");
                let prev_pdu = services()
                    .rooms
                    .get_pdu(prev_event)
                    .ok()
                    .flatten()
                    .ok_or_else(|| {
                        "Could not find prev event, but we know the state.".to_owned()
                    })?;

                if let Some(state_key) = &prev_pdu.state_key {
                    let shortstatekey = services()
                        .rooms
                        .get_or_create_shortstatekey(&prev_pdu.kind.to_string().into(), state_key)
                        .map_err(|_| "Failed to create shortstatekey.".to_owned())?;

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
                let prev_event = if let Ok(Some(pdu)) = services().rooms.get_pdu(prev_eventid) {
                    pdu
                } else {
                    okay = false;
                    break;
                };

                let sstatehash =
                    if let Ok(Some(s)) = services().rooms.pdu_shortstatehash(prev_eventid) {
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
                        .state_full_ids(sstatehash)
                        .await
                        .map_err(|_| "Failed to ask db for room state.".to_owned())?;

                    if let Some(state_key) = &prev_event.state_key {
                        let shortstatekey = services()
                            .rooms
                            .get_or_create_shortstatekey(
                                &prev_event.kind.to_string().into(),
                                state_key,
                            )
                            .map_err(|_| "Failed to create shortstatekey.".to_owned())?;
                        leaf_state.insert(shortstatekey, Arc::from(&*prev_event.event_id));
                        // Now it's the state after the pdu
                    }

                    let mut state = StateMap::with_capacity(leaf_state.len());
                    let mut starting_events = Vec::with_capacity(leaf_state.len());

                    for (k, id) in leaf_state {
                        if let Ok((ty, st_key)) = services().rooms.get_statekey_from_short(k) {
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
                            .get_auth_chain(room_id, starting_events, services())
                            .await
                            .map_err(|_| "Failed to load auth chain.".to_owned())?
                            .collect(),
                    );

                    fork_states.push(state);
                }

                let lock = services().globals.stateres_mutex.lock();

                let result =
                    state_res::resolve(room_version_id, &fork_states, auth_chain_sets, |id| {
                        let res = services().rooms.get_pdu(id);
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
                                let shortstatekey = services()
                                    .rooms
                                    .get_or_create_shortstatekey(
                                        &event_type.to_string().into(),
                                        &state_key,
                                    )
                                    .map_err(|_| {
                                        "Failed to get_or_create_shortstatekey".to_owned()
                                    })?;
                                Ok((shortstatekey, event_id))
                            })
                            .collect::<Result<_, String>>()?,
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
                        let state_key = pdu
                            .state_key
                            .clone()
                            .ok_or_else(|| "Found non-state pdu in state events.".to_owned())?;

                        let shortstatekey = services()
                            .rooms
                            .get_or_create_shortstatekey(&pdu.kind.to_string().into(), &state_key)
                            .map_err(|_| "Failed to create shortstatekey.".to_owned())?;

                        match state.entry(shortstatekey) {
                            btree_map::Entry::Vacant(v) => {
                                v.insert(Arc::from(&*pdu.event_id));
                            }
                            btree_map::Entry::Occupied(_) => return Err(
                                "State event's type and state_key combination exists multiple times."
                                    .to_owned(),
                            ),
                        }
                    }

                    // The original create event must still be in the state
                    let create_shortstatekey = services()
                        .rooms
                        .get_shortstatekey(&StateEventType::RoomCreate, "")
                        .map_err(|_| "Failed to talk to db.")?
                        .expect("Room exists");

                    if state.get(&create_shortstatekey).map(|id| id.as_ref())
                        != Some(&create_event.event_id)
                    {
                        return Err("Incoming event refers to wrong create event.".to_owned());
                    }

                    state_at_incoming_event = Some(state);
                }
                Err(e) => {
                    warn!("Fetching state for event failed: {}", e);
                    return Err("Fetching state for event failed".into());
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
                    .get_shortstatekey(&k.to_string().into(), s)
                    .ok()
                    .flatten()
                    .and_then(|shortstatekey| state_at_incoming_event.get(&shortstatekey))
                    .and_then(|event_id| services().rooms.get_pdu(event_id).ok().flatten())
            },
        )
        .map_err(|_e| "Auth check failed.".to_owned())?;

        if !check_result {
            return Err("Event has failed auth check with state at the event.".into());
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
        let mut extremities = services()
            .rooms
            .get_pdu_leaves(room_id)
            .map_err(|_| "Failed to load room leaves".to_owned())?;

        // Remove any forward extremities that are referenced by this incoming event's prev_events
        for prev_event in &incoming_pdu.prev_events {
            if extremities.contains(prev_event) {
                extremities.remove(prev_event);
            }
        }

        // Only keep those extremities were not referenced yet
        extremities
            .retain(|id| !matches!(services().rooms.is_event_referenced(room_id, id), Ok(true)));

        info!("Compressing state at event");
        let state_ids_compressed = state_at_incoming_event
            .iter()
            .map(|(shortstatekey, id)| {
                services()
                    .rooms
                    .compress_state_event(*shortstatekey, id)
                    .map_err(|_| "Failed to compress_state_event".to_owned())
            })
            .collect::<Result<_, _>>()?;

        // 13. Check if the event passes auth based on the "current state" of the room, if not "soft fail" it
        info!("Starting soft fail auth check");

        let auth_events = services()
            .rooms
            .get_auth_events(
                room_id,
                &incoming_pdu.kind,
                &incoming_pdu.sender,
                incoming_pdu.state_key.as_deref(),
                &incoming_pdu.content,
            )
            .map_err(|_| "Failed to get_auth_events.".to_owned())?;

        let soft_fail = !state_res::event_auth::auth_check(
            &room_version,
            &incoming_pdu,
            None::<PduEvent>,
            |k, s| auth_events.get(&(k.clone(), s.to_owned())),
        )
        .map_err(|_e| "Auth check failed.".to_owned())?;

        if soft_fail {
            self.append_incoming_pdu(
                &incoming_pdu,
                val,
                extremities.iter().map(std::ops::Deref::deref),
                state_ids_compressed,
                soft_fail,
                &state_lock,
            )
            .map_err(|e| {
                warn!("Failed to add pdu to db: {}", e);
                "Failed to add pdu to db.".to_owned()
            })?;

            // Soft fail, we keep the event as an outlier but don't add it to the timeline
            warn!("Event was soft failed: {:?}", incoming_pdu);
            services()
                .rooms
                .mark_event_soft_failed(&incoming_pdu.event_id)
                .map_err(|_| "Failed to set soft failed flag".to_owned())?;
            return Err("Event has been soft failed".into());
        }

        if incoming_pdu.state_key.is_some() {
            info!("Loading current room state ids");
            let current_sstatehash = services()
                .rooms
                .current_shortstatehash(room_id)
                .map_err(|_| "Failed to load current state hash.".to_owned())?
                .expect("every room has state");

            let current_state_ids = services()
                .rooms
                .state_full_ids(current_sstatehash)
                .await
                .map_err(|_| "Failed to load room state.")?;

            info!("Preparing for stateres to derive new room state");
            let mut extremity_sstatehashes = HashMap::new();

            info!("Loading extremities");
            for id in dbg!(&extremities) {
                match services()
                    .rooms
                    .get_pdu(id)
                    .map_err(|_| "Failed to ask db for pdu.".to_owned())?
                {
                    Some(leaf_pdu) => {
                        extremity_sstatehashes.insert(
                            services()
                                .pdu_shortstatehash(&leaf_pdu.event_id)
                                .map_err(|_| "Failed to ask db for pdu state hash.".to_owned())?
                                .ok_or_else(|| {
                                    error!(
                                        "Found extremity pdu with no statehash in db: {:?}",
                                        leaf_pdu
                                    );
                                    "Found pdu with no statehash in db.".to_owned()
                                })?,
                            leaf_pdu,
                        );
                    }
                    _ => {
                        error!("Missing state snapshot for {:?}", id);
                        return Err("Missing state snapshot.".to_owned());
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
                let shortstatekey = services()
                    .rooms
                    .get_or_create_shortstatekey(&incoming_pdu.kind.to_string().into(), state_key)
                    .map_err(|_| "Failed to create shortstatekey.".to_owned())?;

                state_after.insert(shortstatekey, Arc::from(&*incoming_pdu.event_id));
            }
            fork_states.push(state_after);

            let mut update_state = false;
            // 14. Use state resolution to find new room state
            let new_room_state = if fork_states.is_empty() {
                return Err("State is empty.".to_owned());
            } else if fork_states.iter().skip(1).all(|f| &fork_states[0] == f) {
                info!("State resolution trivial");
                // There was only one state, so it has to be the room's current state (because that is
                // always included)
                fork_states[0]
                    .iter()
                    .map(|(k, id)| {
                        services()
                            .rooms
                            .compress_state_event(*k, id)
                            .map_err(|_| "Failed to compress_state_event.".to_owned())
                    })
                    .collect::<Result<_, _>>()?
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
                            .await
                            .map_err(|_| "Failed to load auth chain.".to_owned())?
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
                                    .get_statekey_from_short(k)
                                    // FIXME: Undo .to_string().into() when StateMap
                                    //        is updated to use StateEventType
                                    .map(|(ty, st_key)| ((ty.to_string().into(), st_key), id))
                                    .map_err(|e| warn!("Failed to get_statekey_from_short: {}", e))
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
                        let res = services().rooms.get_pdu(id);
                        if let Err(e) = &res {
                            error!("LOOK AT ME Failed to fetch event: {}", e);
                        }
                        res.ok().flatten()
                    },
                ) {
                    Ok(new_state) => new_state,
                    Err(_) => {
                        return Err("State resolution failed, either an event could not be found or deserialization".into());
                    }
                };

                drop(lock);

                info!("State resolution done. Compressing state");

                state
                    .into_iter()
                    .map(|((event_type, state_key), event_id)| {
                        let shortstatekey = services()
                            .rooms
                            .get_or_create_shortstatekey(&event_type.to_string().into(), &state_key)
                            .map_err(|_| "Failed to get_or_create_shortstatekey".to_owned())?;
                        services()
                            .rooms
                            .compress_state_event(shortstatekey, &event_id)
                            .map_err(|_| "Failed to compress state event".to_owned())
                    })
                    .collect::<Result<_, _>>()?
            };

            // Set the new room state to the resolved state
            if update_state {
                info!("Forcing new room state");
                services()
                    .rooms
                    .force_state(room_id, new_room_state)
                    .map_err(|_| "Failed to set new room state.".to_owned())?;
            }
        }

        info!("Appending pdu to timeline");
        extremities.insert(incoming_pdu.event_id.clone());

        // Now that the event has passed all auth it is added into the timeline.
        // We use the `state_at_event` instead of `state_after` so we accurately
        // represent the state for this event.

        let pdu_id = self
            .append_incoming_pdu(
                &incoming_pdu,
                val,
                extremities.iter().map(std::ops::Deref::deref),
                state_ids_compressed,
                soft_fail,
                &state_lock,
            )
            .map_err(|e| {
                warn!("Failed to add pdu to db: {}", e);
                "Failed to add pdu to db.".to_owned()
            })?;

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
                if let Ok(Some(local_pdu)) = services().rooms.get_pdu(id) {
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

                    if let Ok(Some(_)) = services().rooms.get_pdu(&next_id) {
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
    ) -> Vec<(Arc<EventId>, HashMap<Arc<EventId>, (Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>)>)> {
        let mut graph: HashMap<Arc<EventId>, _> = HashMap::new();
        let mut eventid_info = HashMap::new();
        let mut todo_outlier_stack: Vec<Arc<EventId>> = initial_set;

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
        .map_err(|_| "Error sorting prev events".to_owned())?;

        (sorted, eventid_info)
    }
}
