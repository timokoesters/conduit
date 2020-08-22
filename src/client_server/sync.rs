use super::State;
use crate::{ConduitResult, Database, Error, Ruma};
use ruma::{
    api::client::r0::sync::sync_events,
    events::{room::member::MembershipState, AnySyncEphemeralRoomEvent, EventType},
    Raw, RoomId, UserId,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, tokio};
use std::{
    collections::{hash_map, BTreeMap, HashMap, HashSet},
    convert::TryFrom,
    time::Duration,
};

/// # `GET /_matrix/client/r0/sync`
///
/// Synchronize the client's state with the latest state on the server.
///
/// - This endpoint takes a `since` parameter which should be the `next_batch` value from a
/// previous request.
/// - Calling this endpoint without a `since` parameter will return all recent events, the state
/// of all rooms and more data. This should only be called on the initial login of the device.
/// - To get incremental updates, you can call this endpoint with a `since` parameter. This will
/// return all recent events, state updates and more data that happened since the last /sync
/// request.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/sync", data = "<body>")
)]
pub async fn sync_events_route(
    db: State<'_, Database>,
    body: Ruma<sync_events::Request>,
) -> ConduitResult<sync_events::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

    // TODO: match body.set_presence {
    db.rooms.edus.ping_presence(&sender_id)?;

    // Setup watchers, so if there's no response, we can wait for them
    let watcher = db.watch(sender_id, device_id);

    let next_batch = db.globals.current_count()?.to_string();

    let mut joined_rooms = BTreeMap::new();
    let since = body
        .since
        .clone()
        .and_then(|string| string.parse().ok())
        .unwrap_or(0);

    let mut presence_updates = HashMap::new();
    let mut left_encrypted_users = HashSet::new(); // Users that have left any encrypted rooms the sender was in
    let mut device_list_updates = HashSet::new();
    let mut device_list_left = HashSet::new();

    // Look for device list updates of this account
    device_list_updates.extend(
        db.users
            .keys_changed(&sender_id.to_string(), since, None)
            .filter_map(|r| r.ok()),
    );

    for room_id in db.rooms.rooms_joined(&sender_id) {
        let room_id = room_id?;

        let mut non_timeline_pdus = db
            .rooms
            .pdus_since(&sender_id, &room_id, since)?
            .filter_map(|r| r.ok()); // Filter out buggy events

        // Take the last 10 events for the timeline
        let timeline_pdus = non_timeline_pdus
            .by_ref()
            .rev()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();

        let send_notification_counts = !timeline_pdus.is_empty();

        // They /sync response doesn't always return all messages, so we say the output is
        // limited unless there are events in non_timeline_pdus
        let mut limited = false;

        let mut state_pdus = Vec::new();
        for pdu in non_timeline_pdus {
            if pdu.state_key.is_some() {
                state_pdus.push(pdu);
            }
            limited = true;
        }

        let encrypted_room = db
            .rooms
            .room_state_get(&room_id, &EventType::RoomEncryption, "")?
            .is_some();

        // TODO: optimize this?
        let mut send_member_count = false;
        let mut joined_since_last_sync = false;
        let mut new_encrypted_room = false;
        for (state_key, pdu) in db
            .rooms
            .pdus_since(&sender_id, &room_id, since)?
            .filter_map(|r| r.ok())
            .filter_map(|pdu| Some((pdu.state_key.clone()?, pdu)))
        {
            if pdu.kind == EventType::RoomMember {
                send_member_count = true;

                let content = serde_json::from_value::<
                    Raw<ruma::events::room::member::MemberEventContent>,
                >(pdu.content.clone())
                .expect("Raw::from_value always works")
                .deserialize()
                .map_err(|_| Error::bad_database("Invalid PDU in database."))?;

                if pdu.state_key == Some(sender_id.to_string())
                    && content.membership == MembershipState::Join
                {
                    joined_since_last_sync = true;
                } else if encrypted_room && content.membership == MembershipState::Join {
                    // A new user joined an encrypted room
                    let user_id = UserId::try_from(state_key)
                        .map_err(|_| Error::bad_database("Invalid UserId in member PDU."))?;
                    // Add encryption update if we didn't share an encrypted room already
                    if !share_encrypted_room(&db, &sender_id, &user_id, &room_id) {
                        device_list_updates.insert(user_id);
                    }
                } else if encrypted_room && content.membership == MembershipState::Leave {
                    // Write down users that have left encrypted rooms we are in
                    left_encrypted_users.insert(
                        UserId::try_from(state_key)
                            .map_err(|_| Error::bad_database("Invalid UserId in member PDU."))?,
                    );
                }
            } else if pdu.kind == EventType::RoomEncryption {
                new_encrypted_room = true;
            }
        }

        if joined_since_last_sync && encrypted_room || new_encrypted_room {
            // If the user is in a new encrypted room, give them all joined users
            device_list_updates.extend(
                db.rooms
                    .room_members(&room_id)
                    .filter_map(|user_id| {
                        Some(
                            UserId::try_from(user_id.ok()?.clone())
                                .map_err(|_| {
                                    Error::bad_database("Invalid member event state key in db.")
                                })
                                .ok()?,
                        )
                    })
                    .filter(|user_id| {
                        // Don't send key updates from the sender to the sender
                        sender_id != user_id
                    })
                    .filter(|user_id| {
                        // Only send keys if the sender doesn't share an encrypted room with the target already
                        !share_encrypted_room(&db, sender_id, user_id, &room_id)
                    }),
            );
        }

        // Look for device list updates in this room
        device_list_updates.extend(
            db.users
                .keys_changed(&room_id.to_string(), since, None)
                .filter_map(|r| r.ok()),
        );

        let (joined_member_count, invited_member_count, heroes) = if send_member_count {
            let joined_member_count = db.rooms.room_members(&room_id).count();
            let invited_member_count = db.rooms.room_members_invited(&room_id).count();

            // Recalculate heroes (first 5 members)
            let mut heroes = Vec::new();

            if joined_member_count + invited_member_count <= 5 {
                // Go through all PDUs and for each member event, check if the user is still joined or
                // invited until we have 5 or we reach the end

                for hero in db
                    .rooms
                    .all_pdus(&sender_id, &room_id)?
                    .filter_map(|pdu| pdu.ok()) // Ignore all broken pdus
                    .filter(|pdu| pdu.kind == EventType::RoomMember)
                    .map(|pdu| {
                        let content = serde_json::from_value::<
                            Raw<ruma::events::room::member::MemberEventContent>,
                        >(pdu.content.clone())
                        .expect("Raw::from_value always works")
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid member event in database."))?;

                        if let Some(state_key) = &pdu.state_key {
                            let user_id = UserId::try_from(state_key.clone()).map_err(|_| {
                                Error::bad_database("Invalid UserId in member PDU.")
                            })?;

                            // The membership was and still is invite or join
                            if matches!(
                                content.membership,
                                MembershipState::Join | MembershipState::Invite
                            ) && (db.rooms.is_joined(&user_id, &room_id)?
                                || db.rooms.is_invited(&user_id, &room_id)?)
                            {
                                Ok::<_, Error>(Some(state_key.clone()))
                            } else {
                                Ok(None)
                            }
                        } else {
                            Ok(None)
                        }
                    })
                    .filter_map(|u| u.ok()) // Filter out buggy users
                    // Filter for possible heroes
                    .filter_map(|u| u)
                {
                    if heroes.contains(&hero) || hero == sender_id.as_str() {
                        continue;
                    }

                    heroes.push(hero);
                }
            }

            (
                Some(joined_member_count),
                Some(invited_member_count),
                heroes,
            )
        } else {
            (None, None, Vec::new())
        };

        let notification_count = if send_notification_counts {
            if let Some(last_read) = db.rooms.edus.room_read_get(&room_id, &sender_id)? {
                Some(
                    (db.rooms
                        .pdus_since(&sender_id, &room_id, last_read)?
                        .filter_map(|pdu| pdu.ok()) // Filter out buggy events
                        .filter(|pdu| {
                            matches!(
                                pdu.kind.clone(),
                                EventType::RoomMessage | EventType::RoomEncrypted
                            )
                        })
                        .count() as u32)
                        .into(),
                )
            } else {
                None
            }
        } else {
            None
        };

        let prev_batch = timeline_pdus.first().map_or(Ok::<_, Error>(None), |e| {
            Ok(Some(
                db.rooms
                    .get_pdu_count(&e.event_id)?
                    .ok_or_else(|| Error::bad_database("Can't find count from event in db."))?
                    .to_string(),
            ))
        })?;

        let room_events = timeline_pdus
            .into_iter()
            .map(|pdu| pdu.to_sync_room_event())
            .collect::<Vec<_>>();

        let mut edus = db
            .rooms
            .edus
            .roomlatests_since(&room_id, since)?
            .filter_map(|r| r.ok()) // Filter out buggy events
            .collect::<Vec<_>>();

        if db
            .rooms
            .edus
            .last_roomactive_update(&room_id, &db.globals)?
            > since
        {
            edus.push(
                serde_json::from_str(
                    &serde_json::to_string(&AnySyncEphemeralRoomEvent::Typing(
                        db.rooms.edus.roomactives_all(&room_id)?,
                    ))
                    .expect("event is valid, we just created it"),
                )
                .expect("event is valid, we just created it"),
            );
        }

        let joined_room = sync_events::JoinedRoom {
            account_data: sync_events::AccountData {
                events: db
                    .account_data
                    .changes_since(Some(&room_id), &sender_id, since)?
                    .into_iter()
                    .filter_map(|(_, v)| {
                        serde_json::from_str(v.json().get())
                            .map_err(|_| Error::bad_database("Invalid account event in database."))
                            .ok()
                    })
                    .collect::<Vec<_>>(),
            },
            summary: sync_events::RoomSummary {
                heroes,
                joined_member_count: joined_member_count.map(|n| (n as u32).into()),
                invited_member_count: invited_member_count.map(|n| (n as u32).into()),
            },
            unread_notifications: sync_events::UnreadNotificationsCount {
                highlight_count: None,
                notification_count,
            },
            timeline: sync_events::Timeline {
                limited: limited || joined_since_last_sync,
                prev_batch,
                events: room_events,
            },
            // TODO: state before timeline
            state: sync_events::State {
                events: if joined_since_last_sync {
                    db.rooms
                        .room_state_full(&room_id)?
                        .into_iter()
                        .map(|(_, pdu)| pdu.to_sync_state_event())
                        .collect()
                } else {
                    Vec::new()
                },
            },
            ephemeral: sync_events::Ephemeral { events: edus },
        };

        if !joined_room.is_empty() {
            joined_rooms.insert(room_id.clone(), joined_room);
        }

        // Take presence updates from this room
        for (user_id, presence) in
            db.rooms
                .edus
                .presence_since(&room_id, since, &db.rooms, &db.globals)?
        {
            match presence_updates.entry(user_id) {
                hash_map::Entry::Vacant(v) => {
                    v.insert(presence);
                }
                hash_map::Entry::Occupied(mut o) => {
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

    let mut left_rooms = BTreeMap::new();
    for room_id in db.rooms.rooms_left(&sender_id) {
        let room_id = room_id?;
        let pdus = db.rooms.pdus_since(&sender_id, &room_id, since)?;
        let room_events = pdus
            .filter_map(|pdu| pdu.ok()) // Filter out buggy events
            .map(|pdu| pdu.to_sync_room_event())
            .collect();

        let left_room = sync_events::LeftRoom {
            account_data: sync_events::AccountData { events: Vec::new() },
            timeline: sync_events::Timeline {
                limited: false,
                prev_batch: Some(next_batch.clone()),
                events: room_events,
            },
            state: sync_events::State { events: Vec::new() },
        };

        let mut left_since_last_sync = false;
        for pdu in db.rooms.pdus_since(&sender_id, &room_id, since)? {
            let pdu = pdu?;
            if pdu.kind == EventType::RoomMember && pdu.state_key == Some(sender_id.to_string()) {
                let content = serde_json::from_value::<
                    Raw<ruma::events::room::member::MemberEventContent>,
                >(pdu.content.clone())
                .expect("Raw::from_value always works")
                .deserialize()
                .map_err(|_| Error::bad_database("Invalid PDU in database."))?;

                if content.membership == MembershipState::Leave {
                    left_since_last_sync = true;
                    break;
                }
            }
        }

        if left_since_last_sync {
            device_list_left.extend(
                db.rooms
                    .room_members(&room_id)
                    .filter_map(|user_id| {
                        Some(
                            UserId::try_from(user_id.ok()?.clone())
                                .map_err(|_| {
                                    Error::bad_database("Invalid member event state key in db.")
                                })
                                .ok()?,
                        )
                    })
                    .filter(|user_id| {
                        // Don't send key updates from the sender to the sender
                        sender_id != user_id
                    })
                    .filter(|user_id| {
                        // Only send if the sender doesn't share any encrypted room with the target
                        // anymore
                        !share_encrypted_room(&db, sender_id, user_id, &room_id)
                    }),
            );
        }

        if !left_room.is_empty() {
            left_rooms.insert(room_id.clone(), left_room);
        }
    }

    let mut invited_rooms = BTreeMap::new();
    for room_id in db.rooms.rooms_invited(&sender_id) {
        let room_id = room_id?;
        let mut invited_since_last_sync = false;
        for pdu in db.rooms.pdus_since(&sender_id, &room_id, since)? {
            let pdu = pdu?;
            if pdu.kind == EventType::RoomMember && pdu.state_key == Some(sender_id.to_string()) {
                let content = serde_json::from_value::<
                    Raw<ruma::events::room::member::MemberEventContent>,
                >(pdu.content.clone())
                .expect("Raw::from_value always works")
                .deserialize()
                .map_err(|_| Error::bad_database("Invalid PDU in database."))?;

                if content.membership == MembershipState::Invite {
                    invited_since_last_sync = true;
                    break;
                }
            }
        }

        if !invited_since_last_sync {
            continue;
        }

        let invited_room = sync_events::InvitedRoom {
            invite_state: sync_events::InviteState {
                events: db
                    .rooms
                    .room_state_full(&room_id)?
                    .into_iter()
                    .map(|(_, pdu)| pdu.to_stripped_state_event())
                    .collect(),
            },
        };

        if !invited_room.is_empty() {
            invited_rooms.insert(room_id.clone(), invited_room);
        }
    }

    for user_id in left_encrypted_users {
        // If the user doesn't share an encrypted room with the target anymore, we need to tell
        // them
        if db
            .rooms
            .get_shared_rooms(vec![sender_id.clone(), user_id.clone()])
            .filter_map(|r| r.ok())
            .filter_map(|other_room_id| {
                Some(
                    db.rooms
                        .room_state_get(&other_room_id, &EventType::RoomEncryption, "")
                        .ok()?
                        .is_some(),
                )
            })
            .all(|encrypted| !encrypted)
        {
            device_list_left.insert(user_id);
        }
    }

    // Remove all to-device events the device received *last time*
    db.users
        .remove_to_device_events(sender_id, device_id, since)?;

    let response = sync_events::Response {
        next_batch,
        rooms: sync_events::Rooms {
            leave: left_rooms,
            join: joined_rooms,
            invite: invited_rooms,
        },
        presence: sync_events::Presence {
            events: presence_updates
                .into_iter()
                .map(|(_, v)| Raw::from(v))
                .collect(),
        },
        account_data: sync_events::AccountData {
            events: db
                .account_data
                .changes_since(None, &sender_id, since)?
                .into_iter()
                .filter_map(|(_, v)| {
                    serde_json::from_str(v.json().get())
                        .map_err(|_| Error::bad_database("Invalid account event in database."))
                        .ok()
                })
                .collect::<Vec<_>>(),
        },
        device_lists: sync_events::DeviceLists {
            changed: device_list_updates.into_iter().collect(),
            left: device_list_left.into_iter().collect(),
        },
        device_one_time_keys_count: if db.users.last_one_time_keys_update(sender_id)? > since {
            db.users.count_one_time_keys(sender_id, device_id)?
        } else {
            BTreeMap::new()
        },
        to_device: sync_events::ToDevice {
            events: db.users.get_to_device_events(sender_id, device_id)?,
        },
    };

    // TODO: Retry the endpoint instead of returning (waiting for #118)
    if !body.full_state
        && response.rooms.is_empty()
        && response.presence.is_empty()
        && response.account_data.is_empty()
        && response.device_lists.is_empty()
        && response.device_one_time_keys_count.is_empty()
        && response.to_device.is_empty()
    {
        // Hang a few seconds so requests are not spammed
        // Stop hanging if new info arrives
        let mut duration = body.timeout.unwrap_or_default();
        if duration.as_secs() > 30 {
            duration = Duration::from_secs(30);
        }
        let mut delay = tokio::time::delay_for(duration);
        tokio::select! {
            _ = &mut delay => {}
            _ = watcher => {}
        }
    }

    Ok(response.into())
}

fn share_encrypted_room(
    db: &Database,
    sender_id: &UserId,
    user_id: &UserId,
    ignore_room: &RoomId,
) -> bool {
    db.rooms
        .get_shared_rooms(vec![sender_id.clone(), user_id.clone()])
        .filter_map(|r| r.ok())
        .filter(|room_id| room_id != ignore_room)
        .filter_map(|other_room_id| {
            Some(
                db.rooms
                    .room_state_get(&other_room_id, &EventType::RoomEncryption, "")
                    .ok()?
                    .is_some(),
            )
        })
        .any(|encrypted| encrypted)
}
