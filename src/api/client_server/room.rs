use crate::{
    api::client_server::invite_helper, service::pdu::PduBuilder, services, Error, Result, Ruma,
};
use ruma::{
    api::client::{
        error::ErrorKind,
        room::{self, aliases, create_room, get_room_event, upgrade_room},
    },
    events::{
        room::{
            canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent,
            guest_access::{GuestAccess, RoomGuestAccessEventContent},
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            member::{MembershipState, RoomMemberEventContent},
            name::RoomNameEventContent,
            power_levels::RoomPowerLevelsEventContent,
            tombstone::RoomTombstoneEventContent,
            topic::RoomTopicEventContent,
        },
        StateEventType, TimelineEventType,
    },
    int,
    serde::JsonObject,
    CanonicalJsonObject, CanonicalJsonValue, OwnedRoomAliasId, OwnedUserId, RoomAliasId,
};
use serde::Deserialize;
use serde_json::{json, value::to_raw_value};
use std::{
    cmp::max,
    collections::{BTreeMap, HashSet},
    sync::Arc,
};
use tracing::{error, info, warn};

/// # `POST /_matrix/client/r0/createRoom`
///
/// Creates a new room.
///
/// - Room ID is randomly generated
/// - Create alias if room_alias_name is set
/// - Send create event
/// - Join sender user
/// - Send power levels event
/// - Send canonical room alias
/// - Send join rules
/// - Send history visibility
/// - Send guest access
/// - Send events listed in initial state
/// - Send events implied by `name` and `topic`
/// - Send invite events
pub async fn create_room_route(
    body: Ruma<create_room::v3::Request>,
) -> Result<create_room::v3::Response> {
    use create_room::v3::RoomPreset;

    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !services().globals.allow_room_creation()
        && body.appservice_info.is_none()
        && !services().users.is_admin(sender_user)?
    {
        return Err(Error::BadRequest(
            ErrorKind::forbidden(),
            "Room creation has been disabled.",
        ));
    }

    let alias: Option<OwnedRoomAliasId> =
        body.room_alias_name
            .as_ref()
            .map_or(Ok(None), |localpart| {
                // TODO: Check for invalid characters and maximum length
                let alias = RoomAliasId::parse(format!(
                    "#{}:{}",
                    localpart,
                    services().globals.server_name()
                ))
                .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid alias."))?;

                if services()
                    .rooms
                    .alias
                    .resolve_local_alias(&alias)?
                    .is_some()
                {
                    Err(Error::BadRequest(
                        ErrorKind::RoomInUse,
                        "Room alias already exists.",
                    ))
                } else {
                    Ok(Some(alias))
                }
            })?;

    if let Some(ref alias) = alias {
        if let Some(ref info) = body.appservice_info {
            if !info.aliases.is_match(alias.as_str()) {
                return Err(Error::BadRequest(
                    ErrorKind::Exclusive,
                    "Room alias is not in namespace.",
                ));
            }
        } else if services().appservice.is_exclusive_alias(alias).await {
            return Err(Error::BadRequest(
                ErrorKind::Exclusive,
                "Room alias reserved by appservice.",
            ));
        }
    }

    let room_version = match body.room_version.clone() {
        Some(room_version) => {
            if services()
                .globals
                .supported_room_versions()
                .contains(&room_version)
            {
                room_version
            } else {
                return Err(Error::BadRequest(
                    ErrorKind::UnsupportedRoomVersion,
                    "This server does not support that room version.",
                ));
            }
        }
        None => services().globals.default_room_version(),
    };
    let rules = room_version
        .rules()
        .expect("Supported room version must have rules.")
        .authorization;

    let mut users = BTreeMap::new();
    if !rules.explicitly_privilege_room_creators {
        users.insert(sender_user.clone(), int!(100));
    }

    // Figure out preset. We need it for preset specific events
    let preset = body.preset.clone().unwrap_or(match &body.visibility {
        room::Visibility::Private => RoomPreset::PrivateChat,
        room::Visibility::Public => RoomPreset::PublicChat,
        _ => RoomPreset::PrivateChat, // Room visibility should not be custom
    });

    let mut additional_creators: HashSet<OwnedUserId, _> = HashSet::new();

    if preset == RoomPreset::TrustedPrivateChat {
        if rules.additional_room_creators {
            additional_creators.extend(body.invite.clone())
        } else {
            for invited_user in &body.invite {
                users.insert(invited_user.clone(), int!(100));
            }
        }
    }

    let content = match &body.creation_content {
        Some(raw_content) => {
            let mut content = raw_content
                .deserialize_as_unchecked::<CanonicalJsonObject>()
                .expect("Invalid creation content");

            if !rules.use_room_create_sender {
                content.insert(
                    "creator".into(),
                    json!(&sender_user).try_into().map_err(|_| {
                        Error::BadRequest(ErrorKind::BadJson, "Invalid creation content")
                    })?,
                );
            }

            if rules.additional_room_creators && !additional_creators.is_empty() {
                #[derive(Deserialize)]
                struct AdditionalCreators {
                    additional_creators: Vec<OwnedUserId>,
                }

                if let Ok(AdditionalCreators {
                    additional_creators: ac,
                }) = raw_content.deserialize_as_unchecked()
                {
                    additional_creators.extend(ac);
                }

                content.insert(
                    "additional_creators".into(),
                    json!(&additional_creators).try_into().map_err(|_| {
                        Error::BadRequest(ErrorKind::BadJson, "Invalid additional creators")
                    })?,
                );
            }

            content.insert(
                "room_version".into(),
                json!(room_version.as_str()).try_into().map_err(|_| {
                    Error::BadRequest(ErrorKind::BadJson, "Invalid creation content")
                })?,
            );
            content
        }
        None => {
            let content = RoomCreateEventContent {
                additional_creators: additional_creators.into_iter().collect(),
                room_version,
                ..if rules.use_room_create_sender {
                    RoomCreateEventContent::new_v11()
                } else {
                    RoomCreateEventContent::new_v1(sender_user.clone())
                }
            };

            serde_json::from_str::<CanonicalJsonObject>(
                to_raw_value(&content)
                    .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid creation content"))?
                    .get(),
            )
            .expect("room create event content created by us is valid")
        }
    };

    // Validate creation content
    let de_result = serde_json::from_str::<CanonicalJsonObject>(
        to_raw_value(&content)
            .expect("Invalid creation content")
            .get(),
    );

    if de_result.is_err() {
        return Err(Error::BadRequest(
            ErrorKind::BadJson,
            "Invalid creation content",
        ));
    }

    // 1. The room create event
    let (room_id, mutex_state) = services()
        .rooms
        .timeline
        .send_create_room(
            to_raw_value(&content).expect("event is valid, we just created it"),
            sender_user,
            &rules,
        )
        .await?;
    let state_lock = mutex_state.lock().await;

    // 2. Let the room creator join
    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomMember,
                content: to_raw_value(&RoomMemberEventContent {
                    membership: MembershipState::Join,
                    displayname: services().users.displayname(sender_user)?,
                    avatar_url: services().users.avatar_url(sender_user)?,
                    is_direct: Some(body.is_direct),
                    third_party_invite: None,
                    blurhash: services().users.blurhash(sender_user)?,
                    reason: None,
                    join_authorized_via_users_server: None,
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(sender_user.to_string()),
                redacts: None,
                timestamp: None,
            },
            sender_user,
            &room_id,
            &state_lock,
        )
        .await?;

    // 3. Power levels
    let mut power_levels_content = serde_json::to_value(RoomPowerLevelsEventContent {
        users,
        ..RoomPowerLevelsEventContent::new(&rules)
    })
    .expect("event is valid, we just created it");

    if let Some(power_level_content_override) = &body.power_level_content_override {
        let json: JsonObject = serde_json::from_str(power_level_content_override.json().get())
            .map_err(|_| {
                Error::BadRequest(ErrorKind::BadJson, "Invalid power_level_content_override.")
            })?;

        for (key, value) in json {
            power_levels_content[key] = value;
        }
    }

    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomPowerLevels,
                content: to_raw_value(&power_levels_content)
                    .expect("to_raw_value always works on serde_json::Value"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
                timestamp: None,
            },
            sender_user,
            &room_id,
            &state_lock,
        )
        .await?;

    // 4. Canonical room alias
    if let Some(room_alias_id) = &alias {
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomCanonicalAlias,
                    content: to_raw_value(&RoomCanonicalAliasEventContent {
                        alias: Some(room_alias_id.to_owned()),
                        alt_aliases: vec![],
                    })
                    .expect("We checked that alias earlier, it must be fine"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                sender_user,
                &room_id,
                &state_lock,
            )
            .await?;
    }

    // 5. Events set by preset

    // 5.1 Join Rules
    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomJoinRules,
                content: to_raw_value(&RoomJoinRulesEventContent::new(match preset {
                    RoomPreset::PublicChat => JoinRule::Public,
                    // according to spec "invite" is the default
                    _ => JoinRule::Invite,
                }))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
                timestamp: None,
            },
            sender_user,
            &room_id,
            &state_lock,
        )
        .await?;

    // 5.2 History Visibility
    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomHistoryVisibility,
                content: to_raw_value(&RoomHistoryVisibilityEventContent::new(
                    HistoryVisibility::Shared,
                ))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
                timestamp: None,
            },
            sender_user,
            &room_id,
            &state_lock,
        )
        .await?;

    // 5.3 Guest Access
    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomGuestAccess,
                content: to_raw_value(&RoomGuestAccessEventContent::new(match preset {
                    RoomPreset::PublicChat => GuestAccess::Forbidden,
                    _ => GuestAccess::CanJoin,
                }))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
                timestamp: None,
            },
            sender_user,
            &room_id,
            &state_lock,
        )
        .await?;

    // 6. Events listed in initial_state
    for event in &body.initial_state {
        let mut pdu_builder = event.deserialize_as::<PduBuilder>().map_err(|e| {
            warn!("Invalid initial state event: {:?}", e);
            Error::BadRequest(ErrorKind::InvalidParam, "Invalid initial state event.")
        })?;

        // Implicit state key defaults to ""
        pdu_builder.state_key.get_or_insert_with(|| "".to_owned());

        // Silently skip encryption events if they are not allowed
        if pdu_builder.event_type == TimelineEventType::RoomEncryption
            && !services().globals.allow_encryption()
        {
            continue;
        }

        services()
            .rooms
            .timeline
            .build_and_append_pdu(pdu_builder, sender_user, &room_id, &state_lock)
            .await?;
    }

    // 7. Events implied by name and topic
    if let Some(name) = &body.name {
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomName,
                    content: to_raw_value(&RoomNameEventContent::new(name.clone()))
                        .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                sender_user,
                &room_id,
                &state_lock,
            )
            .await?;
    }

    if let Some(topic) = body.topic.clone() {
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomTopic,
                    content: to_raw_value(&RoomTopicEventContent::new(topic))
                        .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                sender_user,
                &room_id,
                &state_lock,
            )
            .await?;
    }

    // 8. Events implied by invite (and TODO: invite_3pid)
    drop(state_lock);
    for user_id in &body.invite {
        let _ = invite_helper(sender_user, user_id, &room_id, None, body.is_direct).await;
    }

    // Homeserver specific stuff
    if let Some(alias) = alias {
        services()
            .rooms
            .alias
            .set_alias(&alias, &room_id, sender_user)?;
    }

    if body.visibility == room::Visibility::Public {
        services().rooms.directory.set_public(&room_id)?;
    }

    info!("{} created a room", sender_user);

    Ok(create_room::v3::Response::new(room_id))
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/event/{eventId}`
///
/// Gets a single event.
///
/// - You have to currently be joined to the room (TODO: Respect history visibility)
pub async fn get_room_event_route(
    body: Ruma<get_room_event::v3::Request>,
) -> Result<get_room_event::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = services()
        .rooms
        .timeline
        .get_pdu(&body.event_id)?
        .ok_or_else(|| {
            warn!("Event not found, event ID: {:?}", &body.event_id);
            Error::BadRequest(ErrorKind::NotFound, "Event not found.")
        })?;

    if !services().rooms.state_accessor.user_can_see_event(
        sender_user,
        &event.room_id(),
        &body.event_id,
    )? {
        return Err(Error::BadRequest(
            ErrorKind::forbidden(),
            "You don't have permission to view this event.",
        ));
    }

    let mut event = (*event).clone();
    event.add_age()?;

    Ok(get_room_event::v3::Response {
        event: event.to_room_event(),
    })
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/aliases`
///
/// Lists all aliases of the room.
///
/// - Only users joined to the room are allowed to call this TODO: Allow any user to call it if history_visibility is world readable
pub async fn get_room_aliases_route(
    body: Ruma<aliases::v3::Request>,
) -> Result<aliases::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !services()
        .rooms
        .state_cache
        .is_joined(sender_user, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::forbidden(),
            "You don't have permission to view this room.",
        ));
    }

    Ok(aliases::v3::Response {
        aliases: services()
            .rooms
            .alias
            .local_aliases_for_room(&body.room_id)
            .filter_map(|a| a.ok())
            .collect(),
    })
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/upgrade`
///
/// Upgrades the room.
///
/// - Creates a replacement room
/// - Sends a tombstone event into the current room
/// - Sender user joins the room
/// - Transfers some state events
/// - Moves local aliases
/// - Modifies old room power levels to prevent users from speaking
pub async fn upgrade_room_route(
    body: Ruma<upgrade_room::v3::Request>,
) -> Result<upgrade_room::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !services()
        .globals
        .supported_room_versions()
        .contains(&body.new_version)
    {
        return Err(Error::BadRequest(
            ErrorKind::UnsupportedRoomVersion,
            "This server does not support that room version.",
        ));
    }

    let rules = body
        .new_version
        .rules()
        .expect("Supported room version must have rules.")
        .authorization;

    // Get the old room creation event
    let mut create_event_content = serde_json::from_str::<CanonicalJsonObject>(
        services()
            .rooms
            .state_accessor
            .room_state_get(&body.room_id, &StateEventType::RoomCreate, "")?
            .ok_or_else(|| Error::bad_database("Found room without m.room.create event."))?
            .content
            .get(),
    )
    .map_err(|_| Error::bad_database("Invalid room event in database."))?;

    // Use the m.room.tombstone event as the predecessor
    let predecessor = Some(ruma::events::room::create::PreviousRoom::new(
        body.room_id.clone(),
    ));

    // Send a m.room.create event containing a predecessor field and the applicable room_version
    if rules.use_room_create_sender {
        create_event_content.remove("creator");
    } else {
        create_event_content.insert(
            "creator".into(),
            json!(&sender_user).try_into().map_err(|_| {
                Error::BadRequest(ErrorKind::BadJson, "Error forming creation event")
            })?,
        );
    }

    if rules.additional_room_creators && !body.additional_creators.is_empty() {
        create_event_content.insert(
            "additional_creators".into(),
            json!(&body.additional_creators).try_into().map_err(|_| {
                Error::BadRequest(
                    ErrorKind::BadJson,
                    "Failed to convert provided additional additional creators to JSON",
                )
            })?,
        );
    }

    create_event_content.insert(
        "room_version".into(),
        json!(&body.new_version)
            .try_into()
            .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Error forming creation event"))?,
    );
    create_event_content.insert(
        "predecessor".into(),
        json!(predecessor)
            .try_into()
            .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Error forming creation event"))?,
    );

    // Validate creation event content
    let de_result = serde_json::from_str::<CanonicalJsonObject>(
        to_raw_value(&create_event_content)
            .expect("Error forming creation event")
            .get(),
    );

    if de_result.is_err() {
        return Err(Error::BadRequest(
            ErrorKind::BadJson,
            "Error forming creation event",
        ));
    }

    // Lock the room being replaced
    let mutex_state = Arc::clone(
        services()
            .globals
            .roomid_mutex_state
            .write()
            .await
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    // Create a replacement room
    let (replacement_room, mutex_state) = services()
        .rooms
        .timeline
        .send_create_room(
            to_raw_value(&create_event_content).expect("event is valid, we just created it"),
            sender_user,
            &rules,
        )
        .await?;

    // Send a m.room.tombstone event to the old room to indicate that it is not intended to be used any further
    // Fail if the sender does not have the required permissions
    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomTombstone,
                content: to_raw_value(&RoomTombstoneEventContent {
                    body: "This room has been replaced".to_owned(),
                    replacement_room: replacement_room.clone(),
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
                timestamp: None,
            },
            sender_user,
            &body.room_id,
            &state_lock,
        )
        .await?;

    // Change lock to replacement room
    drop(state_lock);
    let state_lock = mutex_state.lock().await;

    // Join the new room
    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomMember,
                content: to_raw_value(&RoomMemberEventContent {
                    membership: MembershipState::Join,
                    displayname: services().users.displayname(sender_user)?,
                    avatar_url: services().users.avatar_url(sender_user)?,
                    is_direct: None,
                    third_party_invite: None,
                    blurhash: services().users.blurhash(sender_user)?,
                    reason: None,
                    join_authorized_via_users_server: None,
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(sender_user.to_string()),
                redacts: None,
                timestamp: None,
            },
            sender_user,
            &replacement_room,
            &state_lock,
        )
        .await?;

    // Recommended transferable state events list from the specs
    let transferable_state_events = vec![
        StateEventType::RoomServerAcl,
        StateEventType::RoomEncryption,
        StateEventType::RoomName,
        StateEventType::RoomAvatar,
        StateEventType::RoomTopic,
        StateEventType::RoomGuestAccess,
        StateEventType::RoomHistoryVisibility,
        StateEventType::RoomJoinRules,
        StateEventType::RoomPowerLevels,
    ];

    // Replicate transferable state events to the new room
    for event_type in transferable_state_events {
        let mut event_content =
            match services()
                .rooms
                .state_accessor
                .room_state_get(&body.room_id, &event_type, "")?
            {
                Some(v) => v.content.clone(),
                None => continue, // Skipping missing events.
            };

        if event_type == StateEventType::RoomPowerLevels && rules.explicitly_privilege_room_creators
        {
            let mut pl_event_content: CanonicalJsonObject =
                serde_json::from_str(event_content.get()).map_err(|e| {
                    error!(
                        "Invalid m.room.power_levels event content in room {}: {e}",
                        body.room_id
                    );
                    Error::BadDatabase("Invalid m.room.power_levels event content in room")
                })?;

            if let Some(CanonicalJsonValue::Object(users)) = pl_event_content.get_mut("users") {
                users.remove(sender_user.as_str());

                if rules.additional_room_creators {
                    for user in &body.additional_creators {
                        users.remove(user.as_str());
                    }
                }
            }

            event_content = to_raw_value(&pl_event_content)
                .expect("Must serialize, only changes made was removing keys")
        }

        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: event_type.to_string().into(),
                    content: event_content,
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                sender_user,
                &replacement_room,
                &state_lock,
            )
            .await?;
    }

    // Moves any local aliases to the new room
    for alias in services()
        .rooms
        .alias
        .local_aliases_for_room(&body.room_id)
        .filter_map(|r| r.ok())
    {
        services()
            .rooms
            .alias
            .set_alias(&alias, &replacement_room, sender_user)?;
    }

    // Get the old room power levels
    let mut power_levels_event_content: RoomPowerLevelsEventContent = serde_json::from_str(
        services()
            .rooms
            .state_accessor
            .room_state_get(&body.room_id, &StateEventType::RoomPowerLevels, "")?
            .ok_or_else(|| Error::bad_database("Found room without m.room.create event."))?
            .content
            .get(),
    )
    .map_err(|_| Error::bad_database("Invalid room event in database."))?;

    // Setting events_default and invite to the greater of 50 and users_default + 1
    let new_level = max(int!(50), power_levels_event_content.users_default + int!(1));
    power_levels_event_content.events_default = new_level;
    power_levels_event_content.invite = new_level;

    // Modify the power levels in the old room to prevent sending of events and inviting new users
    let _ = services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomPowerLevels,
                content: to_raw_value(&power_levels_event_content)
                    .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
                timestamp: None,
            },
            sender_user,
            &body.room_id,
            &state_lock,
        )
        .await?;

    drop(state_lock);

    // Return the replacement room id
    Ok(upgrade_room::v3::Response { replacement_room })
}
