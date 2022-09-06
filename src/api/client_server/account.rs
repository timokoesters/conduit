use std::sync::Arc;

use super::{DEVICE_ID_LENGTH, SESSION_ID_LENGTH, TOKEN_LENGTH};
use crate::{
    utils, Error, Result, Ruma, services,
};
use ruma::{
    api::client::{
        account::{
            change_password, deactivate, get_3pids, get_username_availability, register, whoami,
            ThirdPartyIdRemovalStatus,
        },
        error::ErrorKind,
        uiaa::{AuthFlow, AuthType, UiaaInfo},
    },
    events::{
        room::{
            member::{MembershipState, RoomMemberEventContent},
            message::RoomMessageEventContent,
        },
        GlobalAccountDataEventType, RoomEventType,
    },
    push, UserId,
};
use serde_json::value::to_raw_value;
use tracing::{info, warn};

use register::RegistrationKind;

const RANDOM_USER_ID_LENGTH: usize = 10;

/// # `GET /_matrix/client/r0/register/available`
///
/// Checks if a username is valid and available on this server.
///
/// Conditions for returning true:
/// - The user id is not historical
/// - The server name of the user id matches this server
/// - No user or appservice on this server already claimed this username
///
/// Note: This will not reserve the username, so the username might become invalid when trying to register
pub async fn get_register_available_route(
    body: Ruma<get_username_availability::v3::IncomingRequest>,
) -> Result<get_username_availability::v3::Response> {
    // Validate user id
    let user_id =
        UserId::parse_with_server_name(body.username.to_lowercase(), services().globals.server_name())
            .ok()
            .filter(|user_id| {
                !user_id.is_historical() && user_id.server_name() == services().globals.server_name()
            })
            .ok_or(Error::BadRequest(
                ErrorKind::InvalidUsername,
                "Username is invalid.",
            ))?;

    // Check if username is creative enough
    if services().users.exists(&user_id)? {
        return Err(Error::BadRequest(
            ErrorKind::UserInUse,
            "Desired user ID is already taken.",
        ));
    }

    // TODO add check for appservice namespaces

    // If no if check is true we have an username that's available to be used.
    Ok(get_username_availability::v3::Response { available: true })
}

/// # `POST /_matrix/client/r0/register`
///
/// Register an account on this homeserver.
///
/// You can use [`GET /_matrix/client/r0/register/available`](fn.get_register_available_route.html)
/// to check if the user id is valid and available.
///
/// - Only works if registration is enabled
/// - If type is guest: ignores all parameters except initial_device_display_name
/// - If sender is not appservice: Requires UIAA (but we only use a dummy stage)
/// - If type is not guest and no username is given: Always fails after UIAA check
/// - Creates a new account and populates it with default account data
/// - If `inhibit_login` is false: Creates a device and returns device id and access_token
pub async fn register_route(
    body: Ruma<register::v3::IncomingRequest>,
) -> Result<register::v3::Response> {
    if !services().globals.allow_registration() && !body.from_appservice {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Registration has been disabled.",
        ));
    }

    let is_guest = body.kind == RegistrationKind::Guest;

    let user_id = match (&body.username, is_guest) {
        (Some(username), false) => {
            let proposed_user_id =
                UserId::parse_with_server_name(username.to_lowercase(), services().globals.server_name())
                    .ok()
                    .filter(|user_id| {
                        !user_id.is_historical()
                            && user_id.server_name() == services().globals.server_name()
                    })
                    .ok_or(Error::BadRequest(
                        ErrorKind::InvalidUsername,
                        "Username is invalid.",
                    ))?;
            if services().users.exists(&proposed_user_id)? {
                return Err(Error::BadRequest(
                    ErrorKind::UserInUse,
                    "Desired user ID is already taken.",
                ));
            }
            proposed_user_id
        }
        _ => loop {
            let proposed_user_id = UserId::parse_with_server_name(
                utils::random_string(RANDOM_USER_ID_LENGTH).to_lowercase(),
                services().globals.server_name(),
            )
            .unwrap();
            if !services().users.exists(&proposed_user_id)? {
                break proposed_user_id;
            }
        },
    };

    // UIAA
    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec![AuthType::Dummy],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if !body.from_appservice {
        if let Some(auth) = &body.auth {
            let (worked, uiaainfo) = services().uiaa.try_auth(
                &UserId::parse_with_server_name("", services().globals.server_name())
                    .expect("we know this is valid"),
                "".into(),
                auth,
                &uiaainfo,
            )?;
            if !worked {
                return Err(Error::Uiaa(uiaainfo));
            }
        // Success!
        } else if let Some(json) = body.json_body {
            uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
            services().uiaa.create(
                &UserId::parse_with_server_name("", services().globals.server_name())
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

    let password = if is_guest {
        None
    } else {
        body.password.as_deref()
    };

    // Create user
    services().users.create(&user_id, password)?;

    // Default to pretty displayname
    let displayname = format!("{} ⚡️", user_id.localpart());
    services().users
        .set_displayname(&user_id, Some(displayname.clone()))?;

    // Initial account data
    services().account_data.update(
        None,
        &user_id,
        GlobalAccountDataEventType::PushRules.to_string().into(),
        &ruma::events::push_rules::PushRulesEvent {
            content: ruma::events::push_rules::PushRulesEventContent {
                global: push::Ruleset::server_default(&user_id),
            },
        },
    )?;

    // Inhibit login does not work for guests
    if !is_guest && body.inhibit_login {
        return Ok(register::v3::Response {
            access_token: None,
            user_id,
            device_id: None,
        });
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

    // Create device for this account
    services().users.create_device(
        &user_id,
        &device_id,
        &token,
        body.initial_device_display_name.clone(),
    )?;

    info!("New user {} registered on this server.", user_id);
    services().admin
        .send_message(RoomMessageEventContent::notice_plain(format!(
            "New user {} registered on this server.",
            user_id
        )));

    // If this is the first real user, grant them admin privileges
    // Note: the server user, @conduit:servername, is generated first
    if services().users.count()? == 2 {
        services().admin.make_user_admin(&user_id, displayname).await?;

        warn!("Granting {} admin privileges as the first user", user_id);
    }

    Ok(register::v3::Response {
        access_token: Some(token),
        user_id,
        device_id: Some(device_id),
    })
}

/// # `POST /_matrix/client/r0/account/password`
///
/// Changes the password of this account.
///
/// - Requires UIAA to verify user password
/// - Changes the password of the sender user
/// - The password hash is calculated using argon2 with 32 character salt, the plain password is
/// not saved
///
/// If logout_devices is true it does the following for each device except the sender device:
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip, last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
pub async fn change_password_route(
    body: Ruma<change_password::v3::IncomingRequest>,
) -> Result<change_password::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec![AuthType::Password],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) = services().uiaa.try_auth(
            sender_user,
            sender_device,
            auth,
            &uiaainfo,
        )?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else if let Some(json) = body.json_body {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        services().uiaa
            .create(sender_user, sender_device, &uiaainfo, &json)?;
        return Err(Error::Uiaa(uiaainfo));
    } else {
        return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
    }

    services().users
        .set_password(sender_user, Some(&body.new_password))?;

    if body.logout_devices {
        // Logout all devices except the current one
        for id in services()
            .users
            .all_device_ids(sender_user)
            .filter_map(|id| id.ok())
            .filter(|id| id != sender_device)
        {
            services().users.remove_device(sender_user, &id)?;
        }
    }

    info!("User {} changed their password.", sender_user);
    services().admin
        .send_message(RoomMessageEventContent::notice_plain(format!(
            "User {} changed their password.",
            sender_user
        )));

    Ok(change_password::v3::Response {})
}

/// # `GET _matrix/client/r0/account/whoami`
///
/// Get user_id of the sender user.
///
/// Note: Also works for Application Services
pub async fn whoami_route(
    body: Ruma<whoami::v3::Request>,
) -> Result<whoami::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let device_id = body.sender_device.as_ref().cloned();

    Ok(whoami::v3::Response {
        user_id: sender_user.clone(),
        device_id,
        is_guest: services().users.is_deactivated(&sender_user)?,
    })
}

/// # `POST /_matrix/client/r0/account/deactivate`
///
/// Deactivate sender user account.
///
/// - Leaves all rooms and rejects all invitations
/// - Invalidates all access tokens
/// - Deletes all device metadata (device id, device display name, last seen ip, last seen ts)
/// - Forgets all to-device events
/// - Triggers device list updates
/// - Removes ability to log in again
pub async fn deactivate_route(
    body: Ruma<deactivate::v3::IncomingRequest>,
) -> Result<deactivate::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec![AuthType::Password],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) = services().uiaa.try_auth(
            sender_user,
            sender_device,
            auth,
            &uiaainfo,
        )?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else if let Some(json) = body.json_body {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        services().uiaa
            .create(sender_user, sender_device, &uiaainfo, &json)?;
        return Err(Error::Uiaa(uiaainfo));
    } else {
        return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
    }

    // Make the user leave all rooms before deactivation
    services().rooms.leave_all_rooms(&sender_user).await?;

    // Remove devices and mark account as deactivated
    services().users.deactivate_account(sender_user)?;

    info!("User {} deactivated their account.", sender_user);
    services().admin
        .send_message(RoomMessageEventContent::notice_plain(format!(
            "User {} deactivated their account.",
            sender_user
        )));

    Ok(deactivate::v3::Response {
        id_server_unbind_result: ThirdPartyIdRemovalStatus::NoSupport,
    })
}

/// # `GET _matrix/client/r0/account/3pid`
///
/// Get a list of third party identifiers associated with this account.
///
/// - Currently always returns empty list
pub async fn third_party_route(
    body: Ruma<get_3pids::v3::Request>,
) -> Result<get_3pids::v3::Response> {
    let _sender_user = body.sender_user.as_ref().expect("user is authenticated");

    Ok(get_3pids::v3::Response::new(Vec::new()))
}
