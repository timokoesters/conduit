use super::State;
use crate::{pdu::PduBuilder, ConduitResult, Database, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::room::{self, create_room, get_room_event},
    },
    events::{
        room::{guest_access, history_visibility, join_rules, member, name, topic},
        EventType,
    },
    RoomAliasId, RoomId, RoomVersionId,
};
use std::{collections::BTreeMap, convert::TryFrom};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/createRoom", data = "<body>")
)]
pub fn create_room_route(
    db: State<'_, Database>,
    body: Ruma<create_room::Request>,
) -> ConduitResult<create_room::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    let room_id = RoomId::new(db.globals.server_name());

    let alias = body
        .room_alias_name
        .as_ref()
        .map_or(Ok(None), |localpart| {
            // TODO: Check for invalid characters and maximum length
            let alias =
                RoomAliasId::try_from(format!("#{}:{}", localpart, db.globals.server_name()))
                    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid alias."))?;

            if db.rooms.id_from_alias(&alias)?.is_some() {
                Err(Error::BadRequest(
                    ErrorKind::RoomInUse,
                    "Room alias already exists.",
                ))
            } else {
                Ok(Some(alias))
            }
        })?;

    let mut content = ruma::events::room::create::CreateEventContent::new(sender_id.clone());
    content.federate = body.creation_content.as_ref().map_or(true, |c| c.federate);
    content.predecessor = body
        .creation_content
        .as_ref()
        .and_then(|c| c.predecessor.clone());
    content.room_version = RoomVersionId::Version6;

    // 1. The room create event
    db.rooms.append_pdu(
        PduBuilder {
            room_id: room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomCreate,
            content: serde_json::to_value(content).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    // 2. Let the room creator join
    db.rooms.append_pdu(
        PduBuilder {
            room_id: room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomMember,
            content: serde_json::to_value(member::MemberEventContent {
                membership: member::MembershipState::Join,
                displayname: db.users.displayname(&sender_id)?,
                avatar_url: db.users.avatar_url(&sender_id)?,
                is_direct: body.is_direct,
                third_party_invite: None,
            })
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(sender_id.to_string()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    // 3. Power levels
    let mut users = BTreeMap::new();
    users.insert(sender_id.clone(), 100.into());
    for invite_ in &body.invite {
        users.insert(invite_.clone(), 100.into());
    }

    let power_levels_content = if let Some(power_levels) = &body.power_level_content_override {
        serde_json::from_str(power_levels.json().get()).map_err(|_| {
            Error::BadRequest(ErrorKind::BadJson, "Invalid power_level_content_override.")
        })?
    } else {
        serde_json::to_value(ruma::events::room::power_levels::PowerLevelsEventContent {
            ban: 50.into(),
            events: BTreeMap::new(),
            events_default: 0.into(),
            invite: 50.into(),
            kick: 50.into(),
            redact: 50.into(),
            state_default: 50.into(),
            users,
            users_default: 0.into(),
            notifications: ruma::events::room::power_levels::NotificationPowerLevels {
                room: 50.into(),
            },
        })
        .expect("event is valid, we just created it")
    };
    db.rooms.append_pdu(
        PduBuilder {
            room_id: room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomPowerLevels,
            content: power_levels_content,
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    // 4. Events set by preset

    // Figure out preset. We need it for preset specific events
    let visibility = body.visibility.unwrap_or(room::Visibility::Private);
    let preset = body.preset.unwrap_or_else(|| match visibility {
        room::Visibility::Private => create_room::RoomPreset::PrivateChat,
        room::Visibility::Public => create_room::RoomPreset::PublicChat,
    });

    // 4.1 Join Rules
    db.rooms.append_pdu(
        PduBuilder {
            room_id: room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomJoinRules,
            content: match preset {
                create_room::RoomPreset::PublicChat => serde_json::to_value(
                    join_rules::JoinRulesEventContent::new(join_rules::JoinRule::Public),
                )
                .expect("event is valid, we just created it"),
                // according to spec "invite" is the default
                _ => serde_json::to_value(join_rules::JoinRulesEventContent::new(
                    join_rules::JoinRule::Invite,
                ))
                .expect("event is valid, we just created it"),
            },
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    // 4.2 History Visibility
    db.rooms.append_pdu(
        PduBuilder {
            room_id: room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomHistoryVisibility,
            content: serde_json::to_value(history_visibility::HistoryVisibilityEventContent::new(
                history_visibility::HistoryVisibility::Shared,
            ))
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    // 4.3 Guest Access
    db.rooms.append_pdu(
        PduBuilder {
            room_id: room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomGuestAccess,
            content: match preset {
                create_room::RoomPreset::PublicChat => {
                    serde_json::to_value(guest_access::GuestAccessEventContent::new(
                        guest_access::GuestAccess::Forbidden,
                    ))
                    .expect("event is valid, we just created it")
                }
                _ => serde_json::to_value(guest_access::GuestAccessEventContent::new(
                    guest_access::GuestAccess::CanJoin,
                ))
                .expect("event is valid, we just created it"),
            },
            unsigned: None,
            state_key: Some("".to_owned()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    // 5. Events listed in initial_state
    for create_room::InitialStateEvent {
        event_type,
        state_key,
        content,
    } in &body.initial_state
    {
        // Silently skip encryption events if they are not allowed
        if event_type == &EventType::RoomEncryption && db.globals.encryption_disabled() {
            continue;
        }

        db.rooms.append_pdu(
            PduBuilder {
                room_id: room_id.clone(),
                sender: sender_id.clone(),
                event_type: event_type.clone(),
                content: serde_json::from_str(content.get()).map_err(|_| {
                    Error::BadRequest(ErrorKind::BadJson, "Invalid initial_state content.")
                })?,
                unsigned: None,
                state_key: state_key.clone(),
                redacts: None,
            },
            &db.globals,
            &db.account_data,
        )?;
    }

    // 6. Events implied by name and topic
    if let Some(name) = &body.name {
        db.rooms.append_pdu(
            PduBuilder {
                room_id: room_id.clone(),
                sender: sender_id.clone(),
                event_type: EventType::RoomName,
                content: serde_json::to_value(
                    name::NameEventContent::new(name.clone()).map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidParam, "Name is invalid.")
                    })?,
                )
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &db.globals,
            &db.account_data,
        )?;
    }

    if let Some(topic) = &body.topic {
        db.rooms.append_pdu(
            PduBuilder {
                room_id: room_id.clone(),
                sender: sender_id.clone(),
                event_type: EventType::RoomTopic,
                content: serde_json::to_value(topic::TopicEventContent {
                    topic: topic.clone(),
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &db.globals,
            &db.account_data,
        )?;
    }

    // 7. Events implied by invite (and TODO: invite_3pid)
    for user in &body.invite {
        db.rooms.append_pdu(
            PduBuilder {
                room_id: room_id.clone(),
                sender: sender_id.clone(),
                event_type: EventType::RoomMember,
                content: serde_json::to_value(member::MemberEventContent {
                    membership: member::MembershipState::Invite,
                    displayname: db.users.displayname(&user)?,
                    avatar_url: db.users.avatar_url(&user)?,
                    is_direct: body.is_direct,
                    third_party_invite: None,
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(user.to_string()),
                redacts: None,
            },
            &db.globals,
            &db.account_data,
        )?;
    }

    // Homeserver specific stuff
    if let Some(alias) = alias {
        db.rooms.set_alias(&alias, Some(&room_id), &db.globals)?;
    }

    if let Some(room::Visibility::Public) = body.visibility {
        db.rooms.set_public(&room_id, true)?;
    }

    Ok(create_room::Response::new(room_id).into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/event/<_>", data = "<body>")
)]
pub fn get_room_event_route(
    db: State<'_, Database>,
    body: Ruma<get_room_event::Request>,
) -> ConduitResult<get_room_event::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(sender_id, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    Ok(get_room_event::Response {
        event: db
            .rooms
            .get_pdu(&body.event_id)?
            .ok_or(Error::BadRequest(ErrorKind::NotFound, "Event not found."))?
            .to_room_event(),
    }
    .into())
}
