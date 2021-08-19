use std::{
    collections::BTreeMap,
    convert::{TryFrom, TryInto},
    sync::Arc,
};

use super::{DEVICE_ID_LENGTH, SESSION_ID_LENGTH, TOKEN_LENGTH};
use crate::{database::DatabaseGuard, pdu::PduBuilder, utils, ConduitResult, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::{
            account::{
                change_password, deactivate, get_username_availability, register, whoami,
                ThirdPartyIdRemovalStatus,
            },
            contact::get_contacts,
            uiaa::{AuthFlow, UiaaInfo},
        },
    },
    events::{
        room::{
            canonical_alias, guest_access, history_visibility, join_rules, member, message, name,
            topic,
        },
        EventType,
    },
    identifiers::RoomName,
    push, RoomAliasId, RoomId, RoomVersionId, UserId,
};
use tracing::info;

use register::RegistrationKind;
#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

const GUEST_NAME_LENGTH: usize = 10;

/// # `GET /_matrix/client/r0/register/available`
///
/// Checks if a username is valid and available on this server.
///
/// - Returns true if no user or appservice on this server claimed this username
/// - This will not reserve the username, so the username might become invalid when trying to register
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/register/available", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_register_available_route(
    db: DatabaseGuard,
    body: Ruma<get_username_availability::Request<'_>>,
) -> ConduitResult<get_username_availability::Response> {
    // Validate user id
    let user_id = UserId::parse_with_server_name(body.username.clone(), db.globals.server_name())
        .ok()
        .filter(|user_id| {
            !user_id.is_historical() && user_id.server_name() == db.globals.server_name()
        })
        .ok_or(Error::BadRequest(
            ErrorKind::InvalidUsername,
            "Username is invalid.",
        ))?;

    // Check if username is creative enough
    if db.users.exists(&user_id)? {
        return Err(Error::BadRequest(
            ErrorKind::UserInUse,
            "Desired user ID is already taken.",
        ));
    }

    // TODO add check for appservice namespaces

    // If no if check is true we have an username that's available to be used.
    Ok(get_username_availability::Response { available: true }.into())
}

/// # `POST /_matrix/client/r0/register`
///
/// Register an account on this homeserver.
///
/// - Returns the device id and access_token unless `inhibit_login` is true
/// - When registering a guest account, all parameters except initial_device_display_name will be
/// ignored
/// - Creates a new account and a device for it
/// - The account will be populated with default account data
#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/register", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn register_route(
    db: DatabaseGuard,
    body: Ruma<register::Request<'_>>,
) -> ConduitResult<register::Response> {
    if !db.globals.allow_registration() && !body.from_appservice {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Registration has been disabled.",
        ));
    }

    let is_guest = body.kind == RegistrationKind::Guest;

    let mut missing_username = false;

    // Validate user id
    let user_id = UserId::parse_with_server_name(
        if is_guest {
            utils::random_string(GUEST_NAME_LENGTH)
        } else {
            body.username.clone().unwrap_or_else(|| {
                // If the user didn't send a username field, that means the client is just trying
                // the get an UIAA error to see available flows
                missing_username = true;
                // Just give the user a random name. He won't be able to register with it anyway.
                utils::random_string(GUEST_NAME_LENGTH)
            })
        }
        .to_lowercase(),
        db.globals.server_name(),
    )
    .ok()
    .filter(|user_id| !user_id.is_historical() && user_id.server_name() == db.globals.server_name())
    .ok_or(Error::BadRequest(
        ErrorKind::InvalidUsername,
        "Username is invalid.",
    ))?;

    // Check if username is creative enough
    if !missing_username && db.users.exists(&user_id)? {
        return Err(Error::BadRequest(
            ErrorKind::UserInUse,
            "Desired user ID is already taken.",
        ));
    }

    // UIAA
    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec!["m.login.dummy".to_owned()],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if !body.from_appservice {
        if let Some(auth) = &body.auth {
            let (worked, uiaainfo) = db.uiaa.try_auth(
                &UserId::parse_with_server_name("", db.globals.server_name())
                    .expect("we know this is valid"),
                "".into(),
                auth,
                &uiaainfo,
                &db.users,
                &db.globals,
            )?;
            if !worked {
                return Err(Error::Uiaa(uiaainfo));
            }
        // Success!
        } else if let Some(json) = body.json_body {
            uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
            db.uiaa.create(
                &UserId::parse_with_server_name("", db.globals.server_name())
                    .expect("we know this is valid"),
                "".into(),
                &uiaainfo,
                &json,
            )?;
            return Err(Error::Uiaa(uiaainfo));
        } else {
            return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
        }
    }

    if missing_username {
        return Err(Error::BadRequest(
            ErrorKind::MissingParam,
            "Missing username field.",
        ));
    }

    let password = if is_guest {
        None
    } else {
        body.password.as_deref()
    };

    // Create user
    db.users.create(&user_id, password)?;

    let displayname = format!("{} ⚡️", user_id.localpart());

    db.users
        .set_displayname(&user_id, Some(displayname.clone()))?;

    // Initial data
    db.account_data.update(
        None,
        &user_id,
        EventType::PushRules,
        &ruma::events::push_rules::PushRulesEvent {
            content: ruma::events::push_rules::PushRulesEventContent {
                global: push::Ruleset::server_default(&user_id),
            },
        },
        &db.globals,
    )?;

    if !is_guest && body.inhibit_login {
        return Ok(register::Response {
            access_token: None,
            user_id,
            device_id: None,
        }
        .into());
    }

    // Generate new device id if the user didn't specify one
    let device_id = if is_guest {
        None
    } else {
        body.device_id.clone()
    }
    .unwrap_or_else(|| utils::random_string(DEVICE_ID_LENGTH).into());

    // Generate new token for the device
    let token = utils::random_string(TOKEN_LENGTH);

    // Add device
    db.users.create_device(
        &user_id,
        &device_id,
        &token,
        body.initial_device_display_name.clone(),
    )?;

    // If this is the first user on this server, create the admins room
    if db.users.count()? == 1 {
        // Create a user for the server
        let conduit_user = UserId::parse_with_server_name("conduit", db.globals.server_name())
            .expect("@conduit:server_name is valid");

        db.users.create(&conduit_user, None)?;

        let room_id = RoomId::new(db.globals.server_name());

        db.rooms.get_or_create_shortroomid(&room_id, &db.globals)?;

        let mutex_state = Arc::clone(
            db.globals
                .roomid_mutex_state
                .write()
                .unwrap()
                .entry(room_id.clone())
                .or_default(),
        );
        let state_lock = mutex_state.lock().await;

        let mut content = ruma::events::room::create::CreateEventContent::new(conduit_user.clone());
        content.federate = true;
        content.predecessor = None;
        content.room_version = RoomVersionId::Version6;

        // 1. The room create event
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomCreate,
                content: serde_json::to_value(content).expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;

        // 2. Make conduit bot join
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(member::MemberEventContent {
                    membership: member::MembershipState::Join,
                    displayname: None,
                    avatar_url: None,
                    is_direct: None,
                    third_party_invite: None,
                    blurhash: None,
                    reason: None,
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(conduit_user.to_string()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;

        // 3. Power levels
        let mut users = BTreeMap::new();
        users.insert(conduit_user.clone(), 100.into());
        users.insert(user_id.clone(), 100.into());

        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomPowerLevels,
                content: serde_json::to_value(
                    ruma::events::room::power_levels::PowerLevelsEventContent {
                        users,
                        ..Default::default()
                    },
                )
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;

        // 4.1 Join Rules
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomJoinRules,
                content: serde_json::to_value(join_rules::JoinRulesEventContent::new(
                    join_rules::JoinRule::Invite,
                ))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;

        // 4.2 History Visibility
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomHistoryVisibility,
                content: serde_json::to_value(
                    history_visibility::HistoryVisibilityEventContent::new(
                        history_visibility::HistoryVisibility::Shared,
                    ),
                )
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;

        // 4.3 Guest Access
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomGuestAccess,
                content: serde_json::to_value(guest_access::GuestAccessEventContent::new(
                    guest_access::GuestAccess::Forbidden,
                ))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;

        // 6. Events implied by name and topic
        let room_name =
            Box::<RoomName>::try_from(format!("{} Admin Room", db.globals.server_name()))
                .expect("Room name is valid");
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomName,
                content: serde_json::to_value(name::NameEventContent::new(Some(room_name)))
                    .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;

        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomTopic,
                content: serde_json::to_value(topic::TopicEventContent {
                    topic: format!("Manage {}", db.globals.server_name()),
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;

        // Room alias
        let alias: RoomAliasId = format!("#admins:{}", db.globals.server_name())
            .try_into()
            .expect("#admins:server_name is a valid alias name");

        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomCanonicalAlias,
                content: serde_json::to_value(canonical_alias::CanonicalAliasEventContent {
                    alias: Some(alias.clone()),
                    alt_aliases: Vec::new(),
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;

        db.rooms.set_alias(&alias, Some(&room_id), &db.globals)?;

        // Invite and join the real user
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(member::MemberEventContent {
                    membership: member::MembershipState::Invite,
                    displayname: None,
                    avatar_url: None,
                    is_direct: None,
                    third_party_invite: None,
                    blurhash: None,
                    reason: None,
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(user_id.to_string()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(member::MemberEventContent {
                    membership: member::MembershipState::Join,
                    displayname: Some(displayname),
                    avatar_url: None,
                    is_direct: None,
                    third_party_invite: None,
                    blurhash: None,
                    reason: None,
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(user_id.to_string()),
                redacts: None,
            },
            &user_id,
            &room_id,
            &db,
            &state_lock,
        )?;

        // Send welcome message
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMessage,
                content: serde_json::to_value(message::MessageEventContent::text_html(
                        "Thanks for trying out Conduit! This software is still in development, so expect many bugs and missing features. If you have federation enabled, you can join the Conduit chat room by typing `/join #conduit:matrix.org`. **Important: Please don't join any other Matrix rooms over federation without permission from the room's admins.** Some actions might trigger bugs in other server implementations, breaking the chat for everyone else.".to_owned(),
                        "Thanks for trying out Conduit! This software is still in development, so expect many bugs and missing features. If you have federation enabled, you can join the Conduit chat room by typing <code>/join #conduit:matrix.org</code>. <strong>Important: Please don't join any other Matrix rooms over federation without permission from the room's admins.</strong> Some actions might trigger bugs in other server implementations, breaking the chat for everyone else.".to_owned(),
                ))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: None,
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db,
            &state_lock,
        )?;
    }

    info!("{} registered on this server", user_id);

    db.flush()?;

    Ok(register::Response {
        access_token: Some(token),
        user_id,
        device_id: Some(device_id),
    }
    .into())
}

/// # `POST /_matrix/client/r0/account/password`
///
/// Changes the password of this account.
///
/// - Invalidates all other access tokens if logout_devices is true
/// - Deletes all other devices and most of their data (to-device events, last seen, etc.) if
/// logout_devices is true
#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/account/password", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn change_password_route(
    db: DatabaseGuard,
    body: Ruma<change_password::Request<'_>>,
) -> ConduitResult<change_password::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec!["m.login.password".to_owned()],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) = db.uiaa.try_auth(
            &sender_user,
            sender_device,
            auth,
            &uiaainfo,
            &db.users,
            &db.globals,
        )?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else if let Some(json) = body.json_body {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa
            .create(&sender_user, &sender_device, &uiaainfo, &json)?;
        return Err(Error::Uiaa(uiaainfo));
    } else {
        return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
    }

    db.users
        .set_password(&sender_user, Some(&body.new_password))?;

    if body.logout_devices {
        // Logout all devices except the current one
        for id in db
            .users
            .all_device_ids(&sender_user)
            .filter_map(|id| id.ok())
            .filter(|id| id != sender_device)
        {
            db.users.remove_device(&sender_user, &id)?;
        }
    }

    db.flush()?;

    Ok(change_password::Response {}.into())
}

/// # `GET _matrix/client/r0/account/whoami`
///
/// Get user_id of this account.
///
/// - Also works for Application Services
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/account/whoami", data = "<body>")
)]
#[tracing::instrument(skip(body))]
pub async fn whoami_route(body: Ruma<whoami::Request>) -> ConduitResult<whoami::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    Ok(whoami::Response {
        user_id: sender_user.clone(),
    }
    .into())
}

/// # `POST /_matrix/client/r0/account/deactivate`
///
/// Deactivate this user's account
///
/// - Leaves all rooms and rejects all invitations
/// - Invalidates all access tokens
/// - Deletes all devices
/// - Removes ability to log in again
#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/account/deactivate", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn deactivate_route(
    db: DatabaseGuard,
    body: Ruma<deactivate::Request<'_>>,
) -> ConduitResult<deactivate::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec!["m.login.password".to_owned()],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) = db.uiaa.try_auth(
            &sender_user,
            &sender_device,
            auth,
            &uiaainfo,
            &db.users,
            &db.globals,
        )?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else if let Some(json) = body.json_body {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa
            .create(&sender_user, &sender_device, &uiaainfo, &json)?;
        return Err(Error::Uiaa(uiaainfo));
    } else {
        return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
    }

    // Leave all joined rooms and reject all invitations
    let all_rooms = db
        .rooms
        .rooms_joined(&sender_user)
        .chain(
            db.rooms
                .rooms_invited(&sender_user)
                .map(|t| t.map(|(r, _)| r)),
        )
        .collect::<Vec<_>>();

    for room_id in all_rooms {
        let room_id = room_id?;
        let event = member::MemberEventContent {
            membership: member::MembershipState::Leave,
            displayname: None,
            avatar_url: None,
            is_direct: None,
            third_party_invite: None,
            blurhash: None,
            reason: None,
        };

        let mutex_state = Arc::clone(
            db.globals
                .roomid_mutex_state
                .write()
                .unwrap()
                .entry(room_id.clone())
                .or_default(),
        );
        let state_lock = mutex_state.lock().await;

        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(event).expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(sender_user.to_string()),
                redacts: None,
            },
            &sender_user,
            &room_id,
            &db,
            &state_lock,
        )?;
    }

    // Remove devices and mark account as deactivated
    db.users.deactivate_account(&sender_user)?;

    info!("{} deactivated their account", sender_user);

    db.flush()?;

    Ok(deactivate::Response {
        id_server_unbind_result: ThirdPartyIdRemovalStatus::NoSupport,
    }
    .into())
}

/// # `GET _matrix/client/r0/account/3pid`
///
/// Get a list of third party identifiers associated with this account.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/account/3pid", data = "<body>")
)]
pub async fn third_party_route(
    body: Ruma<get_contacts::Request>,
) -> ConduitResult<get_contacts::Response> {
    let _sender_user = body.sender_user.as_ref().expect("user is authenticated");

    Ok(get_contacts::Response::new(Vec::new()).into())
}
