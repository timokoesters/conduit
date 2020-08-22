use super::{State, DEVICE_ID_LENGTH, SESSION_ID_LENGTH, TOKEN_LENGTH};
use crate::{pdu::PduBuilder, utils, ConduitResult, Database, Error, Ruma};
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
    events::{room::member, EventType},
    UserId,
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
pub fn get_register_available_route(
    db: State<'_, Database>,
    body: Ruma<get_username_availability::Request>,
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
pub fn register_route(
    db: State<'_, Database>,
    body: Ruma<register::Request>,
) -> ConduitResult<register::Response> {
    if db.globals.registration_disabled() {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Registration has been disabled.",
        ));
    }

    let is_guest = matches!(body.kind, Some(RegistrationKind::Guest));

    // Validate user id
    let user_id = UserId::parse_with_server_name(
        if is_guest {
            utils::random_string(GUEST_NAME_LENGTH)
        } else {
            body.username.clone().ok_or_else(|| {
                Error::BadRequest(ErrorKind::MissingParam, "Missing username field.")
            })?
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
    if db.users.exists(&user_id)? {
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
pub fn change_password_route(
    db: State<'_, Database>,
    body: Ruma<change_password::Request>,
) -> ConduitResult<change_password::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

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
            &sender_id,
            device_id,
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
        db.uiaa.create(&sender_id, &device_id, &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
    }

    db.users.set_password(&sender_id, &body.new_password)?;

    // TODO: Read logout_devices field when it's available and respect that, currently not supported in Ruma
    // See: https://github.com/ruma/ruma/issues/107
    // Logout all devices except the current one
    for id in db
        .users
        .all_device_ids(&sender_id)
        .filter_map(|id| id.ok())
        .filter(|id| id != device_id)
    {
        db.users.remove_device(&sender_id, &id)?;
    }

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
pub fn whoami_route(body: Ruma<whoami::Request>) -> ConduitResult<whoami::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");
    Ok(whoami::Response {
        user_id: sender_id.clone(),
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
pub fn deactivate_route(
    db: State<'_, Database>,
    body: Ruma<deactivate::Request>,
) -> ConduitResult<deactivate::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

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
            &sender_id,
            &device_id,
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
        db.uiaa.create(&sender_id, &device_id, &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
    }

    // Leave all joined rooms and reject all invitations
    for room_id in db
        .rooms
        .rooms_joined(&sender_id)
        .chain(db.rooms.rooms_invited(&sender_id))
    {
        let room_id = room_id?;
        let event = member::MemberEventContent {
            membership: member::MembershipState::Leave,
            displayname: None,
            avatar_url: None,
            is_direct: None,
            third_party_invite: None,
        };

        db.rooms.append_pdu(
            PduBuilder {
                room_id: room_id.clone(),
                sender: sender_id.clone(),
                event_type: EventType::RoomMember,
                content: serde_json::to_value(event).expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(sender_id.to_string()),
                redacts: None,
            },
            &db.globals,
            &db.account_data,
        )?;
    }

    // Remove devices and mark account as deactivated
    db.users.deactivate_account(&sender_id)?;

    Ok(deactivate::Response {
        id_server_unbind_result: ThirdPartyIdRemovalStatus::NoSupport,
    }
    .into())
}
