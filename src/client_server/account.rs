use std::{collections::BTreeMap, convert::TryInto};

use super::{State, DEVICE_ID_LENGTH, SESSION_ID_LENGTH, TOKEN_LENGTH};
use crate::{pdu::PduBuilder, utils, ConduitResult, Database, Error, Ruma};
use log::info;
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::{
            account::{
                change_password, deactivate, get_username_availability, register, whoami,
                ThirdPartyIdRemovalStatus,
            },
            uiaa::{AuthFlow, UiaaInfo},
        },
    },
    events::{
        room::canonical_alias,
        room::guest_access,
        room::history_visibility,
        room::join_rules,
        room::member,
        room::name,
        room::{message, topic},
        EventType,
    },
    RoomAliasId, RoomId, RoomVersionId, UserId,
};

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
pub async fn get_register_available_route(
    db: State<'_, Database>,
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
pub async fn register_route(
    db: State<'_, Database>,
    body: Ruma<register::Request<'_>>,
) -> ConduitResult<register::Response> {
    if db.globals.registration_disabled() {
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

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) =
            db.uiaa
                .try_auth(&user_id, "".into(), auth, &uiaainfo, &db.users, &db.globals)?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa.create(&user_id, "".into(), &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
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
        body.password.clone()
    }
    .unwrap_or_default();

    // Create user
    db.users.create(&user_id, &password)?;

    // Initial data
    db.account_data.update(
        None,
        &user_id,
        EventType::PushRules,
        &ruma::events::push_rules::PushRulesEvent {
            content: ruma::events::push_rules::PushRulesEventContent {
                global: crate::push_rules::default_pushrules(&user_id),
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
    if db.users.count() == 1 {
        // Create a user for the server
        let conduit_user = UserId::parse_with_server_name("conduit", db.globals.server_name())
            .expect("@conduit:server_name is valid");

        db.users.create(&conduit_user, "")?;

        let room_id = RoomId::new(db.globals.server_name());

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
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
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
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(conduit_user.to_string()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
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
                    },
                )
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
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
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
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
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
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
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
        )?;

        // 6. Events implied by name and topic
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomName,
                content: serde_json::to_value(
                    name::NameEventContent::new("Admin Room".to_owned()).map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidParam, "Name is invalid.")
                    })?,
                )
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some("".to_owned()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
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
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
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
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
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
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(user_id.to_string()),
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
        )?;
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(member::MemberEventContent {
                    membership: member::MembershipState::Join,
                    displayname: None,
                    avatar_url: None,
                    is_direct: None,
                    third_party_invite: None,
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(user_id.to_string()),
                redacts: None,
            },
            &user_id,
            &room_id,
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
        )?;

        // Send welcome message
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMessage,
                content: serde_json::to_value(message::MessageEventContent::Text(
                    message::TextMessageEventContent {
                        body: "Thanks for trying out Conduit! This software is still in development, so expect many bugs and missing features. If you have federation enabled, you can join the Conduit chat room by typing `/join #conduit:matrix.org`. **Important: Please don't join any other Matrix rooms over federation without permission from the room's admins.** Some actions might trigger bugs in other server implementations, breaking the chat for everyone else.".to_owned(),
                        formatted: Some(message::FormattedBody {
                            format: message::MessageFormat::Html,
                            body: "Thanks for trying out Conduit! This software is still in development, so expect many bugs and missing features. If you have federation enabled, you can join the Conduit chat room by typing <code>/join #conduit:matrix.org</code>. <strong>Important: Please don't join any other Matrix rooms over federation without permission from the room's admins.</strong> Some actions might trigger bugs in other server implementations, breaking the chat for everyone else.".to_owned(),
                        }),
                        relates_to: None,
                    },
                ))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: None,
                redacts: None,
            },
            &conduit_user,
            &room_id,
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
        )?;
    }

    info!("{} registered on this server", user_id);

    db.flush().await?;

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
pub async fn change_password_route(
    db: State<'_, Database>,
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
    } else {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa.create(&sender_user, &sender_device, &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
    }

    db.users.set_password(&sender_user, &body.new_password)?;

    // TODO: Read logout_devices field when it's available and respect that, currently not supported in Ruma
    // See: https://github.com/ruma/ruma/issues/107
    // Logout all devices except the current one
    for id in db
        .users
        .all_device_ids(&sender_user)
        .filter_map(|id| id.ok())
        .filter(|id| id != sender_device)
    {
        db.users.remove_device(&sender_user, &id)?;
    }

    db.flush().await?;

    Ok(change_password::Response.into())
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
pub async fn deactivate_route(
    db: State<'_, Database>,
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
    } else {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa.create(&sender_user, &sender_device, &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
    }

    // Leave all joined rooms and reject all invitations
    for room_id in db
        .rooms
        .rooms_joined(&sender_user)
        .chain(db.rooms.rooms_invited(&sender_user))
    {
        let room_id = room_id?;
        let event = member::MemberEventContent {
            membership: member::MembershipState::Leave,
            displayname: None,
            avatar_url: None,
            is_direct: None,
            third_party_invite: None,
        };

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
            &db.globals,
            &db.sending,
            &db.admin,
            &db.account_data,
        )?;
    }

    // Remove devices and mark account as deactivated
    db.users.deactivate_account(&sender_user)?;

    info!("{} deactivated their account", sender_user);

    db.flush().await?;

    Ok(deactivate::Response {
        id_server_unbind_result: ThirdPartyIdRemovalStatus::NoSupport,
    }
    .into())
}
