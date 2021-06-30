use crate::{database::DatabaseGuard, ConduitResult, Database, Error, Result, Ruma, RumaResponse};
use log::error;
use ruma::{
    api::client::r0::{sync::sync_events, uiaa::UiaaResponse},
    events::{room::member::MembershipState, AnySyncEphemeralRoomEvent, EventType},
    serde::Raw,
    DeviceId, RoomId, UserId,
};
use std::{
    collections::{btree_map::Entry, hash_map, BTreeMap, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    sync::Arc,
    time::Duration,
};
use tokio::sync::watch::Sender;

#[cfg(feature = "conduit_bin")]
use rocket::{get, tokio};

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
#[tracing::instrument(skip(db, body))]
pub async fn sync_events_route(
    db: DatabaseGuard,
    body: Ruma<sync_events::Request<'_>>,
) -> std::result::Result<RumaResponse<sync_events::Response>, RumaResponse<UiaaResponse>> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    let arc_db = Arc::new(db);

    let mut rx = match arc_db
        .globals
        .sync_receivers
        .write()
        .unwrap()
        .entry((sender_user.clone(), sender_device.clone()))
    {
        Entry::Vacant(v) => {
            let (tx, rx) = tokio::sync::watch::channel(None);

            tokio::spawn(sync_helper_wrapper(
                Arc::clone(&arc_db),
                sender_user.clone(),
                sender_device.clone(),
                body.since.clone(),
                body.full_state,
                body.timeout,
                tx,
            ));

            v.insert((body.since.clone(), rx)).1.clone()
        }
        Entry::Occupied(mut o) => {
            if o.get().0 != body.since {
                let (tx, rx) = tokio::sync::watch::channel(None);

                tokio::spawn(sync_helper_wrapper(
                    Arc::clone(&arc_db),
                    sender_user.clone(),
                    sender_device.clone(),
                    body.since.clone(),
                    body.full_state,
                    body.timeout,
                    tx,
                ));

                o.insert((body.since.clone(), rx.clone()));

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

pub async fn sync_helper_wrapper(
    db: Arc<DatabaseGuard>,
    sender_user: UserId,
    sender_device: Box<DeviceId>,
    since: Option<String>,
    full_state: bool,
    timeout: Option<Duration>,
    tx: Sender<Option<ConduitResult<sync_events::Response>>>,
) {
    let r = sync_helper(
        Arc::clone(&db),
        sender_user.clone(),
        sender_device.clone(),
        since.clone(),
        full_state,
        timeout,
    )
    .await;

    if let Ok((_, caching_allowed)) = r {
        if !caching_allowed {
            match db
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

    drop(db);

    let _ = tx.send(Some(r.map(|(r, _)| r.into())));
}

async fn sync_helper(
    db: Arc<DatabaseGuard>,
    sender_user: UserId,
    sender_device: Box<DeviceId>,
    since: Option<String>,
    full_state: bool,
    timeout: Option<Duration>,
    // bool = caching allowed
) -> std::result::Result<(sync_events::Response, bool), Error> {
    // TODO: match body.set_presence {
    db.rooms.edus.ping_presence(&sender_user)?;

    // Setup watchers, so if there's no response, we can wait for them
    let watcher = db.watch(&sender_user, &sender_device);

    let next_batch = db.globals.current_count()?;
    let next_batch_string = next_batch.to_string();

    let mut joined_rooms = BTreeMap::new();
    let since = since
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
            .keys_changed(&sender_user.to_string(), since, None)
            .filter_map(|r| r.ok()),
    );

    for room_id in db.rooms.rooms_joined(&sender_user) {
        let room_id = room_id?;

        let mut non_timeline_pdus = db
            .rooms
            .pdus_until(&sender_user, &room_id, u64::MAX)
            .filter_map(|r| {
                // Filter out buggy events
                if r.is_err() {
                    error!("Bad pdu in pdus_since: {:?}", r);
                }
                r.ok()
            })
            .take_while(|(pduid, _)| {
                db.rooms
                    .pdu_count(pduid)
                    .map_or(false, |count| count > since)
            });

        // Take the last 10 events for the timeline
        let timeline_pdus = non_timeline_pdus
            .by_ref()
            .take(10)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>();

        let send_notification_counts = !timeline_pdus.is_empty()
            || db
                .rooms
                .edus
                .last_privateread_update(&sender_user, &room_id)?
                > since;

        // They /sync response doesn't always return all messages, so we say the output is
        // limited unless there are events in non_timeline_pdus
        let limited = non_timeline_pdus.next().is_some();

        // Database queries:

        let current_shortstatehash = db.rooms.current_shortstatehash(&room_id)?;

        // These type is Option<Option<_>>. The outer Option is None when there is no event between
        // since and the current room state, meaning there should be no updates.
        // The inner Option is None when there is an event, but there is no state hash associated
        // with it. This can happen for the RoomCreate event, so all updates should arrive.
        let first_pdu_before_since = db.rooms.pdus_until(&sender_user, &room_id, since).next();

        let pdus_after_since = db
            .rooms
            .pdus_after(&sender_user, &room_id, since)
            .next()
            .is_some();

        let since_shortstatehash = first_pdu_before_since.as_ref().map(|pdu| {
            db.rooms
                .pdu_shortstatehash(&pdu.as_ref().ok()?.1.event_id)
                .ok()?
        });

        let (
            heroes,
            joined_member_count,
            invited_member_count,
            joined_since_last_sync,
            state_events,
        ) = if pdus_after_since && Some(current_shortstatehash) != since_shortstatehash {
            let current_state = db.rooms.room_state_full(&room_id)?;
            let current_members = current_state
                .iter()
                .filter(|(key, _)| key.0 == EventType::RoomMember)
                .map(|(key, value)| (&key.1, value)) // Only keep state key
                .collect::<Vec<_>>();
            let encrypted_room = current_state
                .get(&(EventType::RoomEncryption, "".to_owned()))
                .is_some();
            let since_state = since_shortstatehash
                .as_ref()
                .map(|since_shortstatehash| {
                    since_shortstatehash
                        .map(|since_shortstatehash| db.rooms.state_full(since_shortstatehash))
                        .transpose()
                })
                .transpose()?;

            let since_encryption = since_state.as_ref().map(|state| {
                state
                    .as_ref()
                    .map(|state| state.get(&(EventType::RoomEncryption, "".to_owned())))
            });

            // Calculations:
            let new_encrypted_room =
                encrypted_room && since_encryption.map_or(true, |encryption| encryption.is_none());

            let send_member_count = since_state.as_ref().map_or(true, |since_state| {
                since_state.as_ref().map_or(true, |since_state| {
                    current_members.len()
                        != since_state
                            .iter()
                            .filter(|(key, _)| key.0 == EventType::RoomMember)
                            .count()
                })
            });

            let since_sender_member = since_state.as_ref().map(|since_state| {
                since_state.as_ref().and_then(|state| {
                    state
                        .get(&(EventType::RoomMember, sender_user.as_str().to_owned()))
                        .and_then(|pdu| {
                            serde_json::from_value::<
                                Raw<ruma::events::room::member::MemberEventContent>,
                            >(pdu.content.clone())
                            .expect("Raw::from_value always works")
                            .deserialize()
                            .map_err(|_| Error::bad_database("Invalid PDU in database."))
                            .ok()
                        })
                })
            });

            if encrypted_room {
                for (user_id, current_member) in current_members {
                    let current_membership = serde_json::from_value::<
                        Raw<ruma::events::room::member::MemberEventContent>,
                    >(current_member.content.clone())
                    .expect("Raw::from_value always works")
                    .deserialize()
                    .map_err(|_| Error::bad_database("Invalid PDU in database."))?
                    .membership;

                    let since_membership =
                        since_state
                            .as_ref()
                            .map_or(MembershipState::Leave, |since_state| {
                                since_state
                                    .as_ref()
                                    .and_then(|since_state| {
                                        since_state
                                            .get(&(EventType::RoomMember, user_id.clone()))
                                            .and_then(|since_member| {
                                                serde_json::from_value::<
                                                Raw<ruma::events::room::member::MemberEventContent>,
                                            >(
                                                since_member.content.clone()
                                            )
                                            .expect("Raw::from_value always works")
                                            .deserialize()
                                            .map_err(|_| {
                                                Error::bad_database("Invalid PDU in database.")
                                            })
                                            .ok()
                                            })
                                    })
                                    .map_or(MembershipState::Leave, |member| member.membership)
                            });

                    let user_id = UserId::try_from(user_id.clone())
                        .map_err(|_| Error::bad_database("Invalid UserId in member PDU."))?;

                    match (since_membership, current_membership) {
                        (MembershipState::Leave, MembershipState::Join) => {
                            // A new user joined an encrypted room
                            if !share_encrypted_room(&db, &sender_user, &user_id, &room_id)? {
                                device_list_updates.insert(user_id);
                            }
                        }
                        // TODO: Remove, this should never happen here, right?
                        (MembershipState::Join, MembershipState::Leave) => {
                            // Write down users that have left encrypted rooms we are in
                            left_encrypted_users.insert(user_id);
                        }
                        _ => {}
                    }
                }
            }

            let joined_since_last_sync = since_sender_member.map_or(true, |member| {
                member.map_or(true, |member| member.membership != MembershipState::Join)
            });

            if joined_since_last_sync && encrypted_room || new_encrypted_room {
                // If the user is in a new encrypted room, give them all joined users
                device_list_updates.extend(
                    db.rooms
                        .room_members(&room_id)
                        .flatten()
                        .filter(|user_id| {
                            // Don't send key updates from the sender to the sender
                            &sender_user != user_id
                        })
                        .filter(|user_id| {
                            // Only send keys if the sender doesn't share an encrypted room with the target already
                            !share_encrypted_room(&db, &sender_user, user_id, &room_id)
                                .unwrap_or(false)
                        }),
                );
            }

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
                        .all_pdus(&sender_user, &room_id)
                        .filter_map(|pdu| pdu.ok()) // Ignore all broken pdus
                        .filter(|(_, pdu)| pdu.kind == EventType::RoomMember)
                        .map(|(_, pdu)| {
                            let content = serde_json::from_value::<
                                ruma::events::room::member::MemberEventContent,
                            >(pdu.content.clone())
                            .map_err(|_| {
                                Error::bad_database("Invalid member event in database.")
                            })?;

                            if let Some(state_key) = &pdu.state_key {
                                let user_id =
                                    UserId::try_from(state_key.clone()).map_err(|_| {
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

                (
                    Some(joined_member_count),
                    Some(invited_member_count),
                    heroes,
                )
            } else {
                (None, None, Vec::new())
            };

            let state_events = if joined_since_last_sync {
                current_state
                    .iter()
                    .map(|(_, pdu)| pdu.to_sync_state_event())
                    .collect()
            } else {
                match since_state {
                    None => Vec::new(),
                    Some(Some(since_state)) => current_state
                        .iter()
                        .filter(|(key, value)| {
                            since_state.get(key).map(|e| &e.event_id) != Some(&value.event_id)
                        })
                        .filter(|(_, value)| {
                            !timeline_pdus.iter().any(|(_, timeline_pdu)| {
                                timeline_pdu.kind == value.kind
                                    && timeline_pdu.state_key == value.state_key
                            })
                        })
                        .map(|(_, pdu)| pdu.to_sync_state_event())
                        .collect(),
                    Some(None) => current_state
                        .iter()
                        .map(|(_, pdu)| pdu.to_sync_state_event())
                        .collect(),
                }
            };

            (
                heroes,
                joined_member_count,
                invited_member_count,
                joined_since_last_sync,
                state_events,
            )
        } else {
            (Vec::new(), None, None, false, Vec::new())
        };

        // Look for device list updates in this room
        device_list_updates.extend(
            db.users
                .keys_changed(&room_id.to_string(), since, None)
                .filter_map(|r| r.ok()),
        );

        let notification_count = if send_notification_counts {
            Some(
                db.rooms
                    .notification_count(&sender_user, &room_id)?
                    .try_into()
                    .expect("notification count can't go that high"),
            )
        } else {
            None
        };

        let highlight_count = if send_notification_counts {
            Some(
                db.rooms
                    .highlight_count(&sender_user, &room_id)?
                    .try_into()
                    .expect("highlight count can't go that high"),
            )
        } else {
            None
        };

        let prev_batch = timeline_pdus
            .first()
            .map_or(Ok::<_, Error>(None), |(pdu_id, _)| {
                Ok(Some(db.rooms.pdu_count(pdu_id)?.to_string()))
            })?;

        let room_events = timeline_pdus
            .iter()
            .map(|(_, pdu)| pdu.to_sync_room_event())
            .collect::<Vec<_>>();

        let mut edus = db
            .rooms
            .edus
            .readreceipts_since(&room_id, since)
            .filter_map(|r| r.ok()) // Filter out buggy events
            .map(|(_, _, v)| v)
            .collect::<Vec<_>>();

        if db.rooms.edus.last_typing_update(&room_id, &db.globals)? > since {
            edus.push(
                serde_json::from_str(
                    &serde_json::to_string(&AnySyncEphemeralRoomEvent::Typing(
                        db.rooms.edus.typings_all(&room_id)?,
                    ))
                    .expect("event is valid, we just created it"),
                )
                .expect("event is valid, we just created it"),
            );
        }

        let joined_room = sync_events::JoinedRoom {
            account_data: sync_events::RoomAccountData {
                events: db
                    .account_data
                    .changes_since(Some(&room_id), &sender_user, since)?
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
                highlight_count,
                notification_count,
            },
            timeline: sync_events::Timeline {
                limited: limited || joined_since_last_sync,
                prev_batch,
                events: room_events,
            },
            state: sync_events::State {
                events: state_events,
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
    for result in db.rooms.rooms_left(&sender_user) {
        let (room_id, left_state_events) = result?;
        let left_count = db.rooms.get_left_count(&room_id, &sender_user)?;

        // Left before last sync
        if Some(since) >= left_count {
            continue;
        }

        left_rooms.insert(
            room_id.clone(),
            sync_events::LeftRoom {
                account_data: sync_events::RoomAccountData { events: Vec::new() },
                timeline: sync_events::Timeline {
                    limited: false,
                    prev_batch: Some(next_batch_string.clone()),
                    events: Vec::new(),
                },
                state: sync_events::State {
                    events: left_state_events,
                },
            },
        );
    }

    let mut invited_rooms = BTreeMap::new();
    for result in db.rooms.rooms_invited(&sender_user) {
        let (room_id, invite_state_events) = result?;
        let invite_count = db.rooms.get_invite_count(&room_id, &sender_user)?;

        // Invited before last sync
        if Some(since) >= invite_count {
            continue;
        }

        invited_rooms.insert(
            room_id.clone(),
            sync_events::InvitedRoom {
                invite_state: sync_events::InviteState {
                    events: invite_state_events,
                },
            },
        );
    }

    for user_id in left_encrypted_users {
        let still_share_encrypted_room = db
            .rooms
            .get_shared_rooms(vec![sender_user.clone(), user_id.clone()])?
            .filter_map(|r| r.ok())
            .filter_map(|other_room_id| {
                Some(
                    db.rooms
                        .room_state_get(&other_room_id, &EventType::RoomEncryption, "")
                        .ok()?
                        .is_some(),
                )
            })
            .all(|encrypted| !encrypted);
        // If the user doesn't share an encrypted room with the target anymore, we need to tell
        // them
        if still_share_encrypted_room {
            device_list_left.insert(user_id);
        }
    }

    // Remove all to-device events the device received *last time*
    db.users
        .remove_to_device_events(&sender_user, &sender_device, since)?;

    let response = sync_events::Response {
        next_batch: next_batch_string,
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
        account_data: sync_events::GlobalAccountData {
            events: db
                .account_data
                .changes_since(None, &sender_user, since)?
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
        device_one_time_keys_count: if db.users.last_one_time_keys_update(&sender_user)? > since
            || since == 0
        {
            db.users.count_one_time_keys(&sender_user, &sender_device)?
        } else {
            BTreeMap::new()
        },
        to_device: sync_events::ToDevice {
            events: db
                .users
                .get_to_device_events(&sender_user, &sender_device)?,
        },
    };

    // TODO: Retry the endpoint instead of returning (waiting for #118)
    if !full_state
        && response.rooms.is_empty()
        && response.presence.is_empty()
        && response.account_data.is_empty()
        && response.device_lists.is_empty()
        && response.device_one_time_keys_count.is_empty()
        && response.to_device.is_empty()
    {
        // Hang a few seconds so requests are not spammed
        // Stop hanging if new info arrives
        let mut duration = timeout.unwrap_or_default();
        if duration.as_secs() > 30 {
            duration = Duration::from_secs(30);
        }
        let _ = tokio::time::timeout(duration, watcher).await;
        Ok((response, false))
    } else {
        Ok((response, since != next_batch)) // Only cache if we made progress
    }
}

#[tracing::instrument(skip(db))]
fn share_encrypted_room(
    db: &Database,
    sender_user: &UserId,
    user_id: &UserId,
    ignore_room: &RoomId,
) -> Result<bool> {
    Ok(db
        .rooms
        .get_shared_rooms(vec![sender_user.clone(), user_id.clone()])?
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
        .any(|encrypted| encrypted))
}
