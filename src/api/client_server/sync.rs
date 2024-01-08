use crate::{
    service::rooms::timeline::PduCount, services, Error, PduEvent, Result, Ruma, RumaResponse,
};
use ruma::{
    api::client::{
        filter::{FilterDefinition, LazyLoadOptions},
        sync::sync_events::{
            self,
            v3::{
                Ephemeral, Filter, GlobalAccountData, InviteState, InvitedRoom, JoinedRoom,
                LeftRoom, Presence, RoomAccountData, RoomSummary, Rooms, State, Timeline, ToDevice,
            },
            v4::SlidingOp,
            DeviceLists, UnreadNotificationsCount,
        },
        uiaa::UiaaResponse,
    },
    events::{
        room::member::{MembershipState, RoomMemberEventContent},
        StateEventType, TimelineEventType,
    },
    serde::Raw,
    uint, DeviceId, OwnedDeviceId, OwnedUserId, RoomId, UInt, UserId,
};
use std::{
    collections::{hash_map::Entry, BTreeMap, BTreeSet, HashMap, HashSet},
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch::Sender;
use tracing::error;

/// # `GET /_matrix/client/r0/sync`
///
/// Synchronize the client's state with the latest state on the server.
///
/// - This endpoint takes a `since` parameter which should be the `next_batch` value from a
/// previous request for incremental syncs.
///
/// Calling this endpoint without a `since` parameter returns:
/// - Some of the most recent events of each timeline
/// - Notification counts for each room
/// - Joined and invited member counts, heroes
/// - All state events
///
/// Calling this endpoint with a `since` parameter from a previous `next_batch` returns:
/// For joined rooms:
/// - Some of the most recent events of each timeline that happened after since
/// - If user joined the room after since: All state events (unless lazy loading is activated) and
/// all device list updates in that room
/// - If the user was already in the room: A list of all events that are in the state now, but were
/// not in the state at `since`
/// - If the state we send contains a member event: Joined and invited member counts, heroes
/// - Device list updates that happened after `since`
/// - If there are events in the timeline we send or the user send updated his read mark: Notification counts
/// - EDUs that are active now (read receipts, typing updates, presence)
/// - TODO: Allow multiple sync streams to support Pantalaimon
///
/// For invited rooms:
/// - If the user was invited after `since`: A subset of the state of the room at the point of the invite
///
/// For left rooms:
/// - If the user left after `since`: prev_batch token, empty state (TODO: subset of the state at the point of the leave)
///
/// - Sync is handled in an async task, multiple requests from the same device with the same
/// `since` will be cached
pub async fn sync_events_route(
    body: Ruma<sync_events::v3::Request>,
) -> Result<sync_events::v3::Response, RumaResponse<UiaaResponse>> {
    let sender_user = body.sender_user.expect("user is authenticated");
    let sender_device = body.sender_device.expect("user is authenticated");
    let body = body.body;

    let mut rx = match services()
        .globals
        .sync_receivers
        .write()
        .unwrap()
        .entry((sender_user.clone(), sender_device.clone()))
    {
        Entry::Vacant(v) => {
            let (tx, rx) = tokio::sync::watch::channel(None);

            v.insert((body.since.to_owned(), rx.clone()));

            tokio::spawn(sync_helper_wrapper(
                sender_user.clone(),
                sender_device.clone(),
                body,
                tx,
            ));

            rx
        }
        Entry::Occupied(mut o) => {
            if o.get().0 != body.since {
                let (tx, rx) = tokio::sync::watch::channel(None);

                o.insert((body.since.clone(), rx.clone()));

                tokio::spawn(sync_helper_wrapper(
                    sender_user.clone(),
                    sender_device.clone(),
                    body,
                    tx,
                ));

                rx
            } else {
                o.get().1.clone()
            }
        }
    };

    let we_have_to_wait = rx.borrow().is_none();
    if we_have_to_wait {
        if let Err(e) = rx.changed().await {
            error!("Error waiting for sync: {}", e);
        }
    }

    let result = match rx
        .borrow()
        .as_ref()
        .expect("When sync channel changes it's always set to some")
    {
        Ok(response) => Ok(response.clone()),
        Err(error) => Err(error.to_response()),
    };

    result
}

async fn sync_helper_wrapper(
    sender_user: OwnedUserId,
    sender_device: OwnedDeviceId,
    body: sync_events::v3::Request,
    tx: Sender<Option<Result<sync_events::v3::Response>>>,
) {
    let since = body.since.clone();

    let r = sync_helper(sender_user.clone(), sender_device.clone(), body).await;

    if let Ok((_, caching_allowed)) = r {
        if !caching_allowed {
            match services()
                .globals
                .sync_receivers
                .write()
                .unwrap()
                .entry((sender_user, sender_device))
            {
                Entry::Occupied(o) => {
                    // Only remove if the device didn't start a different /sync already
                    if o.get().0 == since {
                        o.remove();
                    }
                }
                Entry::Vacant(_) => {}
            }
        }
    }

    let _ = tx.send(Some(r.map(|(r, _)| r)));
}

async fn sync_helper(
    sender_user: OwnedUserId,
    sender_device: OwnedDeviceId,
    body: sync_events::v3::Request,
    // bool = caching allowed
) -> Result<(sync_events::v3::Response, bool), Error> {
    // TODO: match body.set_presence {
    services().rooms.edus.presence.ping_presence(&sender_user)?;

    // Setup watchers, so if there's no response, we can wait for them
    let watcher = services().globals.watch(&sender_user, &sender_device);

    let next_batch = services().globals.current_count()?;
    let next_batchcount = PduCount::Normal(next_batch);
    let next_batch_string = next_batch.to_string();

    // Load filter
    let filter = match body.filter {
        None => FilterDefinition::default(),
        Some(Filter::FilterDefinition(filter)) => filter,
        Some(Filter::FilterId(filter_id)) => services()
            .users
            .get_filter(&sender_user, &filter_id)?
            .unwrap_or_default(),
    };

    let (lazy_load_enabled, lazy_load_send_redundant) = match filter.room.state.lazy_load_options {
        LazyLoadOptions::Enabled {
            include_redundant_members: redundant,
        } => (true, redundant),
        _ => (false, false),
    };

    let full_state = body.full_state;

    let mut joined_rooms = BTreeMap::new();
    let since = body
        .since
        .as_ref()
        .and_then(|string| string.parse().ok())
        .unwrap_or(0);
    let sincecount = PduCount::Normal(since);

    let mut presence_updates = HashMap::new();
    let mut left_encrypted_users = HashSet::new(); // Users that have left any encrypted rooms the sender was in
    let mut device_list_updates = HashSet::new();
    let mut device_list_left = HashSet::new();

    // Look for device list updates of this account
    device_list_updates.extend(
        services()
            .users
            .keys_changed(sender_user.as_ref(), since, None)
            .filter_map(|r| r.ok()),
    );

    let all_joined_rooms = services()
        .rooms
        .state_cache
        .rooms_joined(&sender_user)
        .collect::<Vec<_>>();
    for room_id in all_joined_rooms {
        let room_id = room_id?;
        if let Ok(joined_room) = load_joined_room(
            &sender_user,
            &sender_device,
            &room_id,
            since,
            sincecount,
            next_batch,
            next_batchcount,
            lazy_load_enabled,
            lazy_load_send_redundant,
            full_state,
            &mut device_list_updates,
            &mut left_encrypted_users,
        )
        .await
        {
            if !joined_room.is_empty() {
                joined_rooms.insert(room_id.clone(), joined_room);
            }

            // Take presence updates from this room
            for (user_id, presence) in services()
                .rooms
                .edus
                .presence
                .presence_since(&room_id, since)?
            {
                match presence_updates.entry(user_id) {
                    Entry::Vacant(v) => {
                        v.insert(presence);
                    }
                    Entry::Occupied(mut o) => {
                        let p = o.get_mut();

                        // Update existing presence event with more info
                        p.content.presence = presence.content.presence;
                        if let Some(status_msg) = presence.content.status_msg {
                            p.content.status_msg = Some(status_msg);
                        }
                        if let Some(last_active_ago) = presence.content.last_active_ago {
                            p.content.last_active_ago = Some(last_active_ago);
                        }
                        if let Some(displayname) = presence.content.displayname {
                            p.content.displayname = Some(displayname);
                        }
                        if let Some(avatar_url) = presence.content.avatar_url {
                            p.content.avatar_url = Some(avatar_url);
                        }
                        if let Some(currently_active) = presence.content.currently_active {
                            p.content.currently_active = Some(currently_active);
                        }
                    }
                }
            }
        }
    }

    let mut left_rooms = BTreeMap::new();
    let all_left_rooms: Vec<_> = services()
        .rooms
        .state_cache
        .rooms_left(&sender_user)
        .collect();
    for result in all_left_rooms {
        let (room_id, _) = result?;

        let mut left_state_events = Vec::new();

        {
            // Get and drop the lock to wait for remaining operations to finish
            let mutex_insert = Arc::clone(
                services()
                    .globals
                    .roomid_mutex_insert
                    .write()
                    .unwrap()
                    .entry(room_id.clone())
                    .or_default(),
            );
            let insert_lock = mutex_insert.lock().unwrap();
            drop(insert_lock);
        }

        let left_count = services()
            .rooms
            .state_cache
            .get_left_count(&room_id, &sender_user)?;

        // Left before last sync
        if Some(since) >= left_count {
            continue;
        }

        if !services().rooms.metadata.exists(&room_id)? {
            // This is just a rejected invite, not a room we know
            continue;
        }

        let since_shortstatehash = services()
            .rooms
            .user
            .get_token_shortstatehash(&room_id, since)?;

        let since_state_ids = match since_shortstatehash {
            Some(s) => services().rooms.state_accessor.state_full_ids(s).await?,
            None => HashMap::new(),
        };

        let left_event_id = match services().rooms.state_accessor.room_state_get_id(
            &room_id,
            &StateEventType::RoomMember,
            sender_user.as_str(),
        )? {
            Some(e) => e,
            None => {
                error!("Left room but no left state event");
                continue;
            }
        };

        let left_shortstatehash = match services()
            .rooms
            .state_accessor
            .pdu_shortstatehash(&left_event_id)?
        {
            Some(s) => s,
            None => {
                error!("Leave event has no state");
                continue;
            }
        };

        let mut left_state_ids = services()
            .rooms
            .state_accessor
            .state_full_ids(left_shortstatehash)
            .await?;

        let leave_shortstatekey = services()
            .rooms
            .short
            .get_or_create_shortstatekey(&StateEventType::RoomMember, sender_user.as_str())?;

        left_state_ids.insert(leave_shortstatekey, left_event_id);

        let mut i = 0;
        for (key, id) in left_state_ids {
            if full_state || since_state_ids.get(&key) != Some(&id) {
                let (event_type, state_key) =
                    services().rooms.short.get_statekey_from_short(key)?;

                if !lazy_load_enabled
                    || event_type != StateEventType::RoomMember
                    || full_state
                    // TODO: Delete the following line when this is resolved: https://github.com/vector-im/element-web/issues/22565
                    || *sender_user == state_key
                {
                    let pdu = match services().rooms.timeline.get_pdu(&id)? {
                        Some(pdu) => pdu,
                        None => {
                            error!("Pdu in state not found: {}", id);
                            continue;
                        }
                    };

                    left_state_events.push(pdu.to_sync_state_event());

                    i += 1;
                    if i % 100 == 0 {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }

        left_rooms.insert(
            room_id.clone(),
            LeftRoom {
                account_data: RoomAccountData { events: Vec::new() },
                timeline: Timeline {
                    limited: false,
                    prev_batch: Some(next_batch_string.clone()),
                    events: Vec::new(),
                },
                state: State {
                    events: left_state_events,
                },
            },
        );
    }

    let mut invited_rooms = BTreeMap::new();
    let all_invited_rooms: Vec<_> = services()
        .rooms
        .state_cache
        .rooms_invited(&sender_user)
        .collect();
    for result in all_invited_rooms {
        let (room_id, invite_state_events) = result?;

        {
            // Get and drop the lock to wait for remaining operations to finish
            let mutex_insert = Arc::clone(
                services()
                    .globals
                    .roomid_mutex_insert
                    .write()
                    .unwrap()
                    .entry(room_id.clone())
                    .or_default(),
            );
            let insert_lock = mutex_insert.lock().unwrap();
            drop(insert_lock);
        }

        let invite_count = services()
            .rooms
            .state_cache
            .get_invite_count(&room_id, &sender_user)?;

        // Invited before last sync
        if Some(since) >= invite_count {
            continue;
        }

        invited_rooms.insert(
            room_id.clone(),
            InvitedRoom {
                invite_state: InviteState {
                    events: invite_state_events,
                },
            },
        );
    }

    for user_id in left_encrypted_users {
        let dont_share_encrypted_room = services()
            .rooms
            .user
            .get_shared_rooms(vec![sender_user.clone(), user_id.clone()])?
            .filter_map(|r| r.ok())
            .filter_map(|other_room_id| {
                Some(
                    services()
                        .rooms
                        .state_accessor
                        .room_state_get(&other_room_id, &StateEventType::RoomEncryption, "")
                        .ok()?
                        .is_some(),
                )
            })
            .all(|encrypted| !encrypted);
        // If the user doesn't share an encrypted room with the target anymore, we need to tell
        // them
        if dont_share_encrypted_room {
            device_list_left.insert(user_id);
        }
    }

    // Remove all to-device events the device received *last time*
    services()
        .users
        .remove_to_device_events(&sender_user, &sender_device, since)?;

    let response = sync_events::v3::Response {
        next_batch: next_batch_string,
        rooms: Rooms {
            leave: left_rooms,
            join: joined_rooms,
            invite: invited_rooms,
            knock: BTreeMap::new(), // TODO
        },
        presence: Presence {
            events: presence_updates
                .into_values()
                .map(|v| Raw::new(&v).expect("PresenceEvent always serializes successfully"))
                .collect(),
        },
        account_data: GlobalAccountData {
            events: services()
                .account_data
                .changes_since(None, &sender_user, since)?
                .into_iter()
                .filter_map(|(_, v)| {
                    serde_json::from_str(v.json().get())
                        .map_err(|_| Error::bad_database("Invalid account event in database."))
                        .ok()
                })
                .collect(),
        },
        device_lists: DeviceLists {
            changed: device_list_updates.into_iter().collect(),
            left: device_list_left.into_iter().collect(),
        },
        device_one_time_keys_count: services()
            .users
            .count_one_time_keys(&sender_user, &sender_device)?,
        to_device: ToDevice {
            events: services()
                .users
                .get_to_device_events(&sender_user, &sender_device)?,
        },
        // Fallback keys are not yet supported
        device_unused_fallback_key_types: None,
    };

    // TODO: Retry the endpoint instead of returning (waiting for #118)
    if !full_state
        && response.rooms.is_empty()
        && response.presence.is_empty()
        && response.account_data.is_empty()
        && response.device_lists.is_empty()
        && response.to_device.is_empty()
    {
        // Hang a few seconds so requests are not spammed
        // Stop hanging if new info arrives
        let mut duration = body.timeout.unwrap_or_default();
        if duration.as_secs() > 30 {
            duration = Duration::from_secs(30);
        }
        let _ = tokio::time::timeout(duration, watcher).await;
        Ok((response, false))
    } else {
        Ok((response, since != next_batch)) // Only cache if we made progress
    }
}

#[allow(clippy::too_many_arguments)]
async fn load_joined_room(
    sender_user: &UserId,
    sender_device: &DeviceId,
    room_id: &RoomId,
    since: u64,
    sincecount: PduCount,
    next_batch: u64,
    next_batchcount: PduCount,
    lazy_load_enabled: bool,
    lazy_load_send_redundant: bool,
    full_state: bool,
    device_list_updates: &mut HashSet<OwnedUserId>,
    left_encrypted_users: &mut HashSet<OwnedUserId>,
) -> Result<JoinedRoom> {
    {
        // Get and drop the lock to wait for remaining operations to finish
        // This will make sure the we have all events until next_batch
        let mutex_insert = Arc::clone(
            services()
                .globals
                .roomid_mutex_insert
                .write()
                .unwrap()
                .entry(room_id.to_owned())
                .or_default(),
        );
        let insert_lock = mutex_insert.lock().unwrap();
        drop(insert_lock);
    }

    let (timeline_pdus, limited) = load_timeline(sender_user, room_id, sincecount, 10)?;

    let send_notification_counts = !timeline_pdus.is_empty()
        || services()
            .rooms
            .user
            .last_notification_read(sender_user, room_id)?
            > since;

    let mut timeline_users = HashSet::new();
    for (_, event) in &timeline_pdus {
        timeline_users.insert(event.sender.as_str().to_owned());
    }

    services().rooms.lazy_loading.lazy_load_confirm_delivery(
        sender_user,
        sender_device,
        room_id,
        sincecount,
    )?;

    // Database queries:

    let current_shortstatehash =
        if let Some(s) = services().rooms.state.get_room_shortstatehash(room_id)? {
            s
        } else {
            error!("Room {} has no state", room_id);
            return Err(Error::BadDatabase("Room has no state"));
        };

    let since_shortstatehash = services()
        .rooms
        .user
        .get_token_shortstatehash(room_id, since)?;

    let (heroes, joined_member_count, invited_member_count, joined_since_last_sync, state_events) =
        if timeline_pdus.is_empty() && since_shortstatehash == Some(current_shortstatehash) {
            // No state changes
            (Vec::new(), None, None, false, Vec::new())
        } else {
            // Calculates joined_member_count, invited_member_count and heroes
            let calculate_counts = || {
                let joined_member_count = services()
                    .rooms
                    .state_cache
                    .room_joined_count(room_id)?
                    .unwrap_or(0);
                let invited_member_count = services()
                    .rooms
                    .state_cache
                    .room_invited_count(room_id)?
                    .unwrap_or(0);

                // Recalculate heroes (first 5 members)
                let mut heroes = Vec::new();

                if joined_member_count + invited_member_count <= 5 {
                    // Go through all PDUs and for each member event, check if the user is still joined or
                    // invited until we have 5 or we reach the end

                    for hero in services()
                        .rooms
                        .timeline
                        .all_pdus(sender_user, room_id)?
                        .filter_map(|pdu| pdu.ok()) // Ignore all broken pdus
                        .filter(|(_, pdu)| pdu.kind == TimelineEventType::RoomMember)
                        .map(|(_, pdu)| {
                            let content: RoomMemberEventContent =
                                serde_json::from_str(pdu.content.get()).map_err(|_| {
                                    Error::bad_database("Invalid member event in database.")
                                })?;

                            if let Some(state_key) = &pdu.state_key {
                                let user_id = UserId::parse(state_key.clone()).map_err(|_| {
                                    Error::bad_database("Invalid UserId in member PDU.")
                                })?;

                                // The membership was and still is invite or join
                                if matches!(
                                    content.membership,
                                    MembershipState::Join | MembershipState::Invite
                                ) && (services()
                                    .rooms
                                    .state_cache
                                    .is_joined(&user_id, room_id)?
                                    || services()
                                        .rooms
                                        .state_cache
                                        .is_invited(&user_id, room_id)?)
                                {
                                    Ok::<_, Error>(Some(state_key.clone()))
                                } else {
                                    Ok(None)
                                }
                            } else {
                                Ok(None)
                            }
                        })
                        // Filter out buggy users
                        .filter_map(|u| u.ok())
                        // Filter for possible heroes
                        .flatten()
                    {
                        if heroes.contains(&hero) || hero == sender_user.as_str() {
                            continue;
                        }

                        heroes.push(hero);
                    }
                }

                Ok::<_, Error>((
                    Some(joined_member_count),
                    Some(invited_member_count),
                    heroes,
                ))
            };

            let since_sender_member: Option<RoomMemberEventContent> = since_shortstatehash
                .and_then(|shortstatehash| {
                    services()
                        .rooms
                        .state_accessor
                        .state_get(
                            shortstatehash,
                            &StateEventType::RoomMember,
                            sender_user.as_str(),
                        )
                        .transpose()
                })
                .transpose()?
                .and_then(|pdu| {
                    serde_json::from_str(pdu.content.get())
                        .map_err(|_| Error::bad_database("Invalid PDU in database."))
                        .ok()
                });

            let joined_since_last_sync = since_sender_member
                .map_or(true, |member| member.membership != MembershipState::Join);

            if since_shortstatehash.is_none() || joined_since_last_sync {
                // Probably since = 0, we will do an initial sync

                let (joined_member_count, invited_member_count, heroes) = calculate_counts()?;

                let current_state_ids = services()
                    .rooms
                    .state_accessor
                    .state_full_ids(current_shortstatehash)
                    .await?;

                let mut state_events = Vec::new();
                let mut lazy_loaded = HashSet::new();

                let mut i = 0;
                for (shortstatekey, id) in current_state_ids {
                    let (event_type, state_key) = services()
                        .rooms
                        .short
                        .get_statekey_from_short(shortstatekey)?;

                    if event_type != StateEventType::RoomMember {
                        let pdu = match services().rooms.timeline.get_pdu(&id)? {
                            Some(pdu) => pdu,
                            None => {
                                error!("Pdu in state not found: {}", id);
                                continue;
                            }
                        };
                        state_events.push(pdu);

                        i += 1;
                        if i % 100 == 0 {
                            tokio::task::yield_now().await;
                        }
                    } else if !lazy_load_enabled
                || full_state
                || timeline_users.contains(&state_key)
                // TODO: Delete the following line when this is resolved: https://github.com/vector-im/element-web/issues/22565
                || *sender_user == state_key
                    {
                        let pdu = match services().rooms.timeline.get_pdu(&id)? {
                            Some(pdu) => pdu,
                            None => {
                                error!("Pdu in state not found: {}", id);
                                continue;
                            }
                        };

                        // This check is in case a bad user ID made it into the database
                        if let Ok(uid) = UserId::parse(&state_key) {
                            lazy_loaded.insert(uid);
                        }
                        state_events.push(pdu);

                        i += 1;
                        if i % 100 == 0 {
                            tokio::task::yield_now().await;
                        }
                    }
                }

                // Reset lazy loading because this is an initial sync
                services().rooms.lazy_loading.lazy_load_reset(
                    sender_user,
                    sender_device,
                    room_id,
                )?;

                // The state_events above should contain all timeline_users, let's mark them as lazy
                // loaded.
                services().rooms.lazy_loading.lazy_load_mark_sent(
                    sender_user,
                    sender_device,
                    room_id,
                    lazy_loaded,
                    next_batchcount,
                );

                (
                    heroes,
                    joined_member_count,
                    invited_member_count,
                    true,
                    state_events,
                )
            } else {
                // Incremental /sync
                let since_shortstatehash = since_shortstatehash.unwrap();

                let mut state_events = Vec::new();
                let mut lazy_loaded = HashSet::new();

                if since_shortstatehash != current_shortstatehash {
                    let current_state_ids = services()
                        .rooms
                        .state_accessor
                        .state_full_ids(current_shortstatehash)
                        .await?;
                    let since_state_ids = services()
                        .rooms
                        .state_accessor
                        .state_full_ids(since_shortstatehash)
                        .await?;

                    for (key, id) in current_state_ids {
                        if full_state || since_state_ids.get(&key) != Some(&id) {
                            let pdu = match services().rooms.timeline.get_pdu(&id)? {
                                Some(pdu) => pdu,
                                None => {
                                    error!("Pdu in state not found: {}", id);
                                    continue;
                                }
                            };

                            if pdu.kind == TimelineEventType::RoomMember {
                                match UserId::parse(
                                    pdu.state_key
                                        .as_ref()
                                        .expect("State event has state key")
                                        .clone(),
                                ) {
                                    Ok(state_key_userid) => {
                                        lazy_loaded.insert(state_key_userid);
                                    }
                                    Err(e) => error!("Invalid state key for member event: {}", e),
                                }
                            }

                            state_events.push(pdu);
                            tokio::task::yield_now().await;
                        }
                    }
                }

                for (_, event) in &timeline_pdus {
                    if lazy_loaded.contains(&event.sender) {
                        continue;
                    }

                    if !services().rooms.lazy_loading.lazy_load_was_sent_before(
                        sender_user,
                        sender_device,
                        room_id,
                        &event.sender,
                    )? || lazy_load_send_redundant
                    {
                        if let Some(member_event) = services().rooms.state_accessor.room_state_get(
                            room_id,
                            &StateEventType::RoomMember,
                            event.sender.as_str(),
                        )? {
                            lazy_loaded.insert(event.sender.clone());
                            state_events.push(member_event);
                        }
                    }
                }

                services().rooms.lazy_loading.lazy_load_mark_sent(
                    sender_user,
                    sender_device,
                    room_id,
                    lazy_loaded,
                    next_batchcount,
                );

                let encrypted_room = services()
                    .rooms
                    .state_accessor
                    .state_get(current_shortstatehash, &StateEventType::RoomEncryption, "")?
                    .is_some();

                let since_encryption = services().rooms.state_accessor.state_get(
                    since_shortstatehash,
                    &StateEventType::RoomEncryption,
                    "",
                )?;

                // Calculations:
                let new_encrypted_room = encrypted_room && since_encryption.is_none();

                let send_member_count = state_events
                    .iter()
                    .any(|event| event.kind == TimelineEventType::RoomMember);

                if encrypted_room {
                    for state_event in &state_events {
                        if state_event.kind != TimelineEventType::RoomMember {
                            continue;
                        }

                        if let Some(state_key) = &state_event.state_key {
                            let user_id = UserId::parse(state_key.clone()).map_err(|_| {
                                Error::bad_database("Invalid UserId in member PDU.")
                            })?;

                            if user_id == sender_user {
                                continue;
                            }

                            let new_membership = serde_json::from_str::<RoomMemberEventContent>(
                                state_event.content.get(),
                            )
                            .map_err(|_| Error::bad_database("Invalid PDU in database."))?
                            .membership;

                            match new_membership {
                                MembershipState::Join => {
                                    // A new user joined an encrypted room
                                    if !share_encrypted_room(sender_user, &user_id, room_id)? {
                                        device_list_updates.insert(user_id);
                                    }
                                }
                                MembershipState::Leave => {
                                    // Write down users that have left encrypted rooms we are in
                                    left_encrypted_users.insert(user_id);
                                }
                                _ => {}
                            }
                        }
                    }
                }

                if joined_since_last_sync && encrypted_room || new_encrypted_room {
                    // If the user is in a new encrypted room, give them all joined users
                    device_list_updates.extend(
                        services()
                            .rooms
                            .state_cache
                            .room_members(room_id)
                            .flatten()
                            .filter(|user_id| {
                                // Don't send key updates from the sender to the sender
                                sender_user != user_id
                            })
                            .filter(|user_id| {
                                // Only send keys if the sender doesn't share an encrypted room with the target already
                                !share_encrypted_room(sender_user, user_id, room_id)
                                    .unwrap_or(false)
                            }),
                    );
                }

                let (joined_member_count, invited_member_count, heroes) = if send_member_count {
                    calculate_counts()?
                } else {
                    (None, None, Vec::new())
                };

                (
                    heroes,
                    joined_member_count,
                    invited_member_count,
                    joined_since_last_sync,
                    state_events,
                )
            }
        };

    // Look for device list updates in this room
    device_list_updates.extend(
        services()
            .users
            .keys_changed(room_id.as_ref(), since, None)
            .filter_map(|r| r.ok()),
    );

    let notification_count = if send_notification_counts {
        Some(
            services()
                .rooms
                .user
                .notification_count(sender_user, room_id)?
                .try_into()
                .expect("notification count can't go that high"),
        )
    } else {
        None
    };

    let highlight_count = if send_notification_counts {
        Some(
            services()
                .rooms
                .user
                .highlight_count(sender_user, room_id)?
                .try_into()
                .expect("highlight count can't go that high"),
        )
    } else {
        None
    };

    let prev_batch = timeline_pdus
        .first()
        .map_or(Ok::<_, Error>(None), |(pdu_count, _)| {
            Ok(Some(match pdu_count {
                PduCount::Backfilled(_) => {
                    error!("timeline in backfill state?!");
                    "0".to_owned()
                }
                PduCount::Normal(c) => c.to_string(),
            }))
        })?;

    let room_events: Vec<_> = timeline_pdus
        .iter()
        .map(|(_, pdu)| pdu.to_sync_room_event())
        .collect();

    let mut edus: Vec<_> = services()
        .rooms
        .edus
        .read_receipt
        .readreceipts_since(room_id, since)
        .filter_map(|r| r.ok()) // Filter out buggy events
        .map(|(_, _, v)| v)
        .collect();

    if services().rooms.edus.typing.last_typing_update(room_id)? > since {
        edus.push(
            serde_json::from_str(
                &serde_json::to_string(&services().rooms.edus.typing.typings_all(room_id)?)
                    .expect("event is valid, we just created it"),
            )
            .expect("event is valid, we just created it"),
        );
    }

    // Save the state after this sync so we can send the correct state diff next sync
    services().rooms.user.associate_token_shortstatehash(
        room_id,
        next_batch,
        current_shortstatehash,
    )?;

    Ok(JoinedRoom {
        account_data: RoomAccountData {
            events: services()
                .account_data
                .changes_since(Some(room_id), sender_user, since)?
                .into_iter()
                .filter_map(|(_, v)| {
                    serde_json::from_str(v.json().get())
                        .map_err(|_| Error::bad_database("Invalid account event in database."))
                        .ok()
                })
                .collect(),
        },
        summary: RoomSummary {
            heroes,
            joined_member_count: joined_member_count.map(|n| (n as u32).into()),
            invited_member_count: invited_member_count.map(|n| (n as u32).into()),
        },
        unread_notifications: UnreadNotificationsCount {
            highlight_count,
            notification_count,
        },
        timeline: Timeline {
            limited: limited || joined_since_last_sync,
            prev_batch,
            events: room_events,
        },
        state: State {
            events: state_events
                .iter()
                .map(|pdu| pdu.to_sync_state_event())
                .collect(),
        },
        ephemeral: Ephemeral { events: edus },
        unread_thread_notifications: BTreeMap::new(),
    })
}

fn load_timeline(
    sender_user: &UserId,
    room_id: &RoomId,
    roomsincecount: PduCount,
    limit: u64,
) -> Result<(Vec<(PduCount, PduEvent)>, bool), Error> {
    let timeline_pdus;
    let limited;
    if services()
        .rooms
        .timeline
        .last_timeline_count(sender_user, room_id)?
        > roomsincecount
    {
        let mut non_timeline_pdus = services()
            .rooms
            .timeline
            .pdus_until(sender_user, room_id, PduCount::max())?
            .filter_map(|r| {
                // Filter out buggy events
                if r.is_err() {
                    error!("Bad pdu in pdus_since: {:?}", r);
                }
                r.ok()
            })
            .take_while(|(pducount, _)| pducount > &roomsincecount);

        // Take the last events for the timeline
        timeline_pdus = non_timeline_pdus
            .by_ref()
            .take(limit as usize)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();

        // They /sync response doesn't always return all messages, so we say the output is
        // limited unless there are events in non_timeline_pdus
        limited = non_timeline_pdus.next().is_some();
    } else {
        timeline_pdus = Vec::new();
        limited = false;
    }
    Ok((timeline_pdus, limited))
}

fn share_encrypted_room(
    sender_user: &UserId,
    user_id: &UserId,
    ignore_room: &RoomId,
) -> Result<bool> {
    Ok(services()
        .rooms
        .user
        .get_shared_rooms(vec![sender_user.to_owned(), user_id.to_owned()])?
        .filter_map(|r| r.ok())
        .filter(|room_id| room_id != ignore_room)
        .filter_map(|other_room_id| {
            Some(
                services()
                    .rooms
                    .state_accessor
                    .room_state_get(&other_room_id, &StateEventType::RoomEncryption, "")
                    .ok()?
                    .is_some(),
            )
        })
        .any(|encrypted| encrypted))
}

pub async fn sync_events_v4_route(
    body: Ruma<sync_events::v4::Request>,
) -> Result<sync_events::v4::Response, RumaResponse<UiaaResponse>> {
    let sender_user = body.sender_user.expect("user is authenticated");
    let sender_device = body.sender_device.expect("user is authenticated");
    let mut body = body.body;
    // Setup watchers, so if there's no response, we can wait for them
    let watcher = services().globals.watch(&sender_user, &sender_device);

    let next_batch = services().globals.next_count()?;

    let globalsince = body
        .pos
        .as_ref()
        .and_then(|string| string.parse().ok())
        .unwrap_or(0);

    if globalsince == 0 {
        if let Some(conn_id) = &body.conn_id {
            services().users.forget_sync_request_connection(
                sender_user.clone(),
                sender_device.clone(),
                conn_id.clone(),
            )
        }
    }

    // Get sticky parameters from cache
    let known_rooms = services().users.update_sync_request_with_cache(
        sender_user.clone(),
        sender_device.clone(),
        &mut body,
    );

    let all_joined_rooms = services()
        .rooms
        .state_cache
        .rooms_joined(&sender_user)
        .filter_map(|r| r.ok())
        .collect::<Vec<_>>();

    if body.extensions.to_device.enabled.unwrap_or(false) {
        services()
            .users
            .remove_to_device_events(&sender_user, &sender_device, globalsince)?;
    }

    let mut left_encrypted_users = HashSet::new(); // Users that have left any encrypted rooms the sender was in
    let mut device_list_changes = HashSet::new();
    let mut device_list_left = HashSet::new();

    if body.extensions.e2ee.enabled.unwrap_or(false) {
        // Look for device list updates of this account
        device_list_changes.extend(
            services()
                .users
                .keys_changed(sender_user.as_ref(), globalsince, None)
                .filter_map(|r| r.ok()),
        );

        for room_id in &all_joined_rooms {
            let current_shortstatehash =
                if let Some(s) = services().rooms.state.get_room_shortstatehash(room_id)? {
                    s
                } else {
                    error!("Room {} has no state", room_id);
                    continue;
                };

            let since_shortstatehash = services()
                .rooms
                .user
                .get_token_shortstatehash(room_id, globalsince)?;

            let since_sender_member: Option<RoomMemberEventContent> = since_shortstatehash
                .and_then(|shortstatehash| {
                    services()
                        .rooms
                        .state_accessor
                        .state_get(
                            shortstatehash,
                            &StateEventType::RoomMember,
                            sender_user.as_str(),
                        )
                        .transpose()
                })
                .transpose()?
                .and_then(|pdu| {
                    serde_json::from_str(pdu.content.get())
                        .map_err(|_| Error::bad_database("Invalid PDU in database."))
                        .ok()
                });

            let encrypted_room = services()
                .rooms
                .state_accessor
                .state_get(current_shortstatehash, &StateEventType::RoomEncryption, "")?
                .is_some();

            if let Some(since_shortstatehash) = since_shortstatehash {
                // Skip if there are only timeline changes
                if since_shortstatehash == current_shortstatehash {
                    continue;
                }

                let since_encryption = services().rooms.state_accessor.state_get(
                    since_shortstatehash,
                    &StateEventType::RoomEncryption,
                    "",
                )?;

                let joined_since_last_sync = since_sender_member
                    .map_or(true, |member| member.membership != MembershipState::Join);

                let new_encrypted_room = encrypted_room && since_encryption.is_none();
                if encrypted_room {
                    let current_state_ids = services()
                        .rooms
                        .state_accessor
                        .state_full_ids(current_shortstatehash)
                        .await?;
                    let since_state_ids = services()
                        .rooms
                        .state_accessor
                        .state_full_ids(since_shortstatehash)
                        .await?;

                    for (key, id) in current_state_ids {
                        if since_state_ids.get(&key) != Some(&id) {
                            let pdu = match services().rooms.timeline.get_pdu(&id)? {
                                Some(pdu) => pdu,
                                None => {
                                    error!("Pdu in state not found: {}", id);
                                    continue;
                                }
                            };
                            if pdu.kind == TimelineEventType::RoomMember {
                                if let Some(state_key) = &pdu.state_key {
                                    let user_id =
                                        UserId::parse(state_key.clone()).map_err(|_| {
                                            Error::bad_database("Invalid UserId in member PDU.")
                                        })?;

                                    if user_id == sender_user {
                                        continue;
                                    }

                                    let new_membership = serde_json::from_str::<
                                        RoomMemberEventContent,
                                    >(
                                        pdu.content.get()
                                    )
                                    .map_err(|_| Error::bad_database("Invalid PDU in database."))?
                                    .membership;

                                    match new_membership {
                                        MembershipState::Join => {
                                            // A new user joined an encrypted room
                                            if !share_encrypted_room(
                                                &sender_user,
                                                &user_id,
                                                room_id,
                                            )? {
                                                device_list_changes.insert(user_id);
                                            }
                                        }
                                        MembershipState::Leave => {
                                            // Write down users that have left encrypted rooms we are in
                                            left_encrypted_users.insert(user_id);
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                    }
                    if joined_since_last_sync || new_encrypted_room {
                        // If the user is in a new encrypted room, give them all joined users
                        device_list_changes.extend(
                            services()
                                .rooms
                                .state_cache
                                .room_members(room_id)
                                .flatten()
                                .filter(|user_id| {
                                    // Don't send key updates from the sender to the sender
                                    &sender_user != user_id
                                })
                                .filter(|user_id| {
                                    // Only send keys if the sender doesn't share an encrypted room with the target already
                                    !share_encrypted_room(&sender_user, user_id, room_id)
                                        .unwrap_or(false)
                                }),
                        );
                    }
                }
            }
            // Look for device list updates in this room
            device_list_changes.extend(
                services()
                    .users
                    .keys_changed(room_id.as_ref(), globalsince, None)
                    .filter_map(|r| r.ok()),
            );
        }
        for user_id in left_encrypted_users {
            let dont_share_encrypted_room = services()
                .rooms
                .user
                .get_shared_rooms(vec![sender_user.clone(), user_id.clone()])?
                .filter_map(|r| r.ok())
                .filter_map(|other_room_id| {
                    Some(
                        services()
                            .rooms
                            .state_accessor
                            .room_state_get(&other_room_id, &StateEventType::RoomEncryption, "")
                            .ok()?
                            .is_some(),
                    )
                })
                .all(|encrypted| !encrypted);
            // If the user doesn't share an encrypted room with the target anymore, we need to tell
            // them
            if dont_share_encrypted_room {
                device_list_left.insert(user_id);
            }
        }
    }

    let mut lists = BTreeMap::new();
    let mut todo_rooms = BTreeMap::new(); // and required state

    for (list_id, list) in body.lists {
        if list.filters.and_then(|f| f.is_invite).unwrap_or(false) {
            continue;
        }

        let mut new_known_rooms = BTreeSet::new();

        lists.insert(
            list_id.clone(),
            sync_events::v4::SyncList {
                ops: list
                    .ranges
                    .into_iter()
                    .map(|mut r| {
                        r.0 =
                            r.0.clamp(uint!(0), UInt::from(all_joined_rooms.len() as u32 - 1));
                        r.1 =
                            r.1.clamp(r.0, UInt::from(all_joined_rooms.len() as u32 - 1));
                        let room_ids = all_joined_rooms
                            [(u64::from(r.0) as usize)..=(u64::from(r.1) as usize)]
                            .to_vec();
                        new_known_rooms.extend(room_ids.iter().cloned());
                        for room_id in &room_ids {
                            let todo_room = todo_rooms.entry(room_id.clone()).or_insert((
                                BTreeSet::new(),
                                0,
                                u64::MAX,
                            ));
                            let limit = list
                                .room_details
                                .timeline_limit
                                .map_or(10, u64::from)
                                .min(100);
                            todo_room
                                .0
                                .extend(list.room_details.required_state.iter().cloned());
                            todo_room.1 = todo_room.1.max(limit);
                            // 0 means unknown because it got out of date
                            todo_room.2 = todo_room.2.min(
                                known_rooms
                                    .get(&list_id)
                                    .and_then(|k| k.get(room_id))
                                    .copied()
                                    .unwrap_or(0),
                            );
                        }
                        sync_events::v4::SyncOp {
                            op: SlidingOp::Sync,
                            range: Some(r),
                            index: None,
                            room_ids,
                            room_id: None,
                        }
                    })
                    .collect(),
                count: UInt::from(all_joined_rooms.len() as u32),
            },
        );

        if let Some(conn_id) = &body.conn_id {
            services().users.update_sync_known_rooms(
                sender_user.clone(),
                sender_device.clone(),
                conn_id.clone(),
                list_id,
                new_known_rooms,
                globalsince,
            );
        }
    }

    let mut known_subscription_rooms = BTreeSet::new();
    for (room_id, room) in &body.room_subscriptions {
        let todo_room = todo_rooms
            .entry(room_id.clone())
            .or_insert((BTreeSet::new(), 0, u64::MAX));
        let limit = room.timeline_limit.map_or(10, u64::from).min(100);
        todo_room.0.extend(room.required_state.iter().cloned());
        todo_room.1 = todo_room.1.max(limit);
        // 0 means unknown because it got out of date
        todo_room.2 = todo_room.2.min(
            known_rooms
                .get("subscriptions")
                .and_then(|k| k.get(room_id))
                .copied()
                .unwrap_or(0),
        );
        known_subscription_rooms.insert(room_id.clone());
    }

    for r in body.unsubscribe_rooms {
        known_subscription_rooms.remove(&r);
        body.room_subscriptions.remove(&r);
    }

    if let Some(conn_id) = &body.conn_id {
        services().users.update_sync_known_rooms(
            sender_user.clone(),
            sender_device.clone(),
            conn_id.clone(),
            "subscriptions".to_owned(),
            known_subscription_rooms,
            globalsince,
        );
    }

    if let Some(conn_id) = &body.conn_id {
        services().users.update_sync_subscriptions(
            sender_user.clone(),
            sender_device.clone(),
            conn_id.clone(),
            body.room_subscriptions,
        );
    }

    let mut rooms = BTreeMap::new();
    for (room_id, (required_state_request, timeline_limit, roomsince)) in &todo_rooms {
        let roomsincecount = PduCount::Normal(*roomsince);

        let (timeline_pdus, limited) =
            load_timeline(&sender_user, room_id, roomsincecount, *timeline_limit)?;

        if roomsince != &0 && timeline_pdus.is_empty() {
            continue;
        }

        let prev_batch = timeline_pdus
            .first()
            .map_or(Ok::<_, Error>(None), |(pdu_count, _)| {
                Ok(Some(match pdu_count {
                    PduCount::Backfilled(_) => {
                        error!("timeline in backfill state?!");
                        "0".to_owned()
                    }
                    PduCount::Normal(c) => c.to_string(),
                }))
            })?
            .or_else(|| {
                if roomsince != &0 {
                    Some(roomsince.to_string())
                } else {
                    None
                }
            });

        let room_events: Vec<_> = timeline_pdus
            .iter()
            .map(|(_, pdu)| pdu.to_sync_room_event())
            .collect();

        let required_state = required_state_request
            .iter()
            .flat_map(|state| {
                services()
                    .rooms
                    .state_accessor
                    .room_state_get(room_id, &state.0, &state.1)
                    .ok()
                    .flatten()
                    .map(|state| state.to_sync_state_event())
            })
            .collect();

        // Heroes
        let heroes = services()
            .rooms
            .state_cache
            .room_members(room_id)
            .filter_map(|r| r.ok())
            .filter(|member| member != &sender_user)
            .flat_map(|member| {
                services()
                    .rooms
                    .state_accessor
                    .get_member(room_id, &member)
                    .ok()
                    .flatten()
                    .map(|memberevent| {
                        (
                            memberevent
                                .displayname
                                .unwrap_or_else(|| member.to_string()),
                            memberevent.avatar_url,
                        )
                    })
            })
            .take(5)
            .collect::<Vec<_>>();
        let name = match &heroes[..] {
            [] => None,
            [only] => Some(only.0.clone()),
            [firsts @ .., last] => Some(
                firsts
                    .iter()
                    .map(|h| h.0.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
                    + " and "
                    + &last.0,
            ),
        };

        let avatar = if let [only] = &heroes[..] {
            only.1.clone()
        } else {
            None
        };

        rooms.insert(
            room_id.clone(),
            sync_events::v4::SlidingSyncRoom {
                name: services()
                    .rooms
                    .state_accessor
                    .get_name(room_id)?
                    .or_else(|| name),
                avatar: if let Some(avatar) = avatar {
                    ruma::JsOption::Some(avatar)
                } else {
                    match services().rooms.state_accessor.get_avatar(room_id)? {
                        ruma::JsOption::Some(avatar) => {
                            js_option::JsOption::Some(avatar.url.unwrap())
                        }
                        ruma::JsOption::Null => ruma::JsOption::Null,
                        ruma::JsOption::Undefined => ruma::JsOption::Undefined,
                    }
                },
                initial: Some(roomsince == &0),
                is_dm: None,
                invite_state: None,
                unread_notifications: UnreadNotificationsCount {
                    highlight_count: Some(
                        services()
                            .rooms
                            .user
                            .highlight_count(&sender_user, room_id)?
                            .try_into()
                            .expect("notification count can't go that high"),
                    ),
                    notification_count: Some(
                        services()
                            .rooms
                            .user
                            .notification_count(&sender_user, room_id)?
                            .try_into()
                            .expect("notification count can't go that high"),
                    ),
                },
                timeline: room_events,
                required_state,
                prev_batch,
                limited,
                joined_count: Some(
                    (services()
                        .rooms
                        .state_cache
                        .room_joined_count(room_id)?
                        .unwrap_or(0) as u32)
                        .into(),
                ),
                invited_count: Some(
                    (services()
                        .rooms
                        .state_cache
                        .room_invited_count(room_id)?
                        .unwrap_or(0) as u32)
                        .into(),
                ),
                num_live: None, // Count events in timeline greater than global sync counter
                timestamp: None,
            },
        );
    }

    if rooms
        .iter()
        .all(|(_, r)| r.timeline.is_empty() && r.required_state.is_empty())
    {
        // Hang a few seconds so requests are not spammed
        // Stop hanging if new info arrives
        let mut duration = body.timeout.unwrap_or(Duration::from_secs(30));
        if duration.as_secs() > 30 {
            duration = Duration::from_secs(30);
        }
        let _ = tokio::time::timeout(duration, watcher).await;
    }

    Ok(sync_events::v4::Response {
        initial: globalsince == 0,
        txn_id: body.txn_id.clone(),
        pos: next_batch.to_string(),
        lists,
        rooms,
        extensions: sync_events::v4::Extensions {
            to_device: if body.extensions.to_device.enabled.unwrap_or(false) {
                Some(sync_events::v4::ToDevice {
                    events: services()
                        .users
                        .get_to_device_events(&sender_user, &sender_device)?,
                    next_batch: next_batch.to_string(),
                })
            } else {
                None
            },
            e2ee: sync_events::v4::E2EE {
                device_lists: DeviceLists {
                    changed: device_list_changes.into_iter().collect(),
                    left: device_list_left.into_iter().collect(),
                },
                device_one_time_keys_count: services()
                    .users
                    .count_one_time_keys(&sender_user, &sender_device)?,
                // Fallback keys are not yet supported
                device_unused_fallback_key_types: None,
            },
            account_data: sync_events::v4::AccountData {
                global: if body.extensions.account_data.enabled.unwrap_or(false) {
                    services()
                        .account_data
                        .changes_since(None, &sender_user, globalsince)?
                        .into_iter()
                        .filter_map(|(_, v)| {
                            serde_json::from_str(v.json().get())
                                .map_err(|_| {
                                    Error::bad_database("Invalid account event in database.")
                                })
                                .ok()
                        })
                        .collect()
                } else {
                    Vec::new()
                },
                rooms: BTreeMap::new(),
            },
            receipts: sync_events::v4::Receipts {
                rooms: BTreeMap::new(),
            },
            typing: sync_events::v4::Typing {
                rooms: BTreeMap::new(),
            },
        },
        delta_token: None,
    })
}
