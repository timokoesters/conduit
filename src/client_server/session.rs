use super::{DEVICE_ID_LENGTH, TOKEN_LENGTH};
use crate::{database::DatabaseGuard, utils, ConduitResult, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::{
            session::{get_login_types, login, logout, logout_all},
            uiaa::IncomingUserIdentifier,
        },
    },
    UserId,
};
use serde::Deserialize;
use tracing::info;

#[derive(Debug, Deserialize)]
struct Claims {
    sub: String,
    exp: usize,
}

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

/// # `GET /_matrix/client/r0/login`
///
/// Get the supported login types of this server. One of these should be used as the `type` field
/// when logging in.
#[cfg_attr(feature = "conduit_bin", get("/_matrix/client/r0/login"))]
#[tracing::instrument]
pub async fn get_login_types_route() -> ConduitResult<get_login_types::Response> {
    Ok(
        get_login_types::Response::new(vec![get_login_types::LoginType::Password(
            Default::default(),
        )])
        .into(),
    )
}

/// # `POST /_matrix/client/r0/login`
///
/// Authenticates the user and returns an access token it can use in subsequent requests.
///
/// - The user needs to authenticate using their password (or if enabled using a json web token)
/// - If `device_id` is known: invalidates old access token of that device
/// - If `device_id` is unknown: creates a new device
/// - Returns access token that is associated with the user and device
///
/// Note: You can use [`GET /_matrix/client/r0/login`](fn.get_supported_versions_route.html) to see
/// supported login types.
#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/login", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn login_route(
    db: DatabaseGuard,
    body: Ruma<login::Request<'_>>,
) -> ConduitResult<login::Response> {
    // Validate login method
    // TODO: Other login methods
    let user_id = match &body.login_info {
        login::IncomingLoginInfo::Password {
            identifier,
            password,
        } => {
            let username = if let IncomingUserIdentifier::MatrixId(matrix_id) = identifier {
                matrix_id
            } else {
                return Err(Error::BadRequest(ErrorKind::Forbidden, "Bad login type."));
            };
            let user_id =
                UserId::parse_with_server_name(username.to_owned(), db.globals.server_name())
                    .map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidUsername, "Username is invalid.")
                    })?;
            let hash = db.users.password_hash(&user_id)?.ok_or(Error::BadRequest(
                ErrorKind::Forbidden,
                "Wrong username or password.",
            ))?;

            if hash.is_empty() {
                return Err(Error::BadRequest(
                    ErrorKind::UserDeactivated,
                    "The user has been deactivated",
                ));
            }

            let hash_matches = argon2::verify_encoded(&hash, password.as_bytes()).unwrap_or(false);

            if !hash_matches {
                return Err(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "Wrong username or password.",
                ));
            }

            user_id
        }
        login::IncomingLoginInfo::Token { token } => {
            if let Some(jwt_decoding_key) = db.globals.jwt_decoding_key() {
                let token = jsonwebtoken::decode::<Claims>(
                    &token,
                    &jwt_decoding_key,
                    &jsonwebtoken::Validation::default(),
                )
                .map_err(|_| Error::BadRequest(ErrorKind::InvalidUsername, "Token is invalid."))?;
                let username = token.claims.sub;
                UserId::parse_with_server_name(username, db.globals.server_name()).map_err(
                    |_| Error::BadRequest(ErrorKind::InvalidUsername, "Username is invalid."),
                )?
            } else {
                return Err(Error::BadRequest(
                    ErrorKind::Unknown,
                    "Token login is not supported (server has no jwt decoding key).",
                ));
            }
        }
    };

    // Generate new device id if the user didn't specify one
    let device_id = body
        .device_id
        .clone()
        .unwrap_or_else(|| utils::random_string(DEVICE_ID_LENGTH).into());

    // Generate a new token for the device
    let token = utils::random_string(TOKEN_LENGTH);

    // Determine if device_id was provided and exists in the db for this user
    let device_exists = body.device_id.as_ref().map_or(false, |device_id| {
        db.users
            .all_device_ids(&user_id)
            .any(|x| x.as_ref().map_or(false, |v| v == device_id))
    });

    if device_exists {
        db.users.set_token(&user_id, &device_id, &token)?;
    } else {
        db.users.create_device(
            &user_id,
            &device_id,
            &token,
            body.initial_device_display_name.clone(),
        )?;
    }

    info!("{} logged in", user_id);

    db.flush()?;

    Ok(login::Response {
        user_id,
        access_token: token,
        home_server: Some(db.globals.server_name().to_owned()),
        device_id,
        well_known: None,
    }
    .into())
}

/// # `POST /_matrix/client/r0/logout`
///
/// Log out the current device.
///
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip, last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/logout", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn logout_route(
    db: DatabaseGuard,
    body: Ruma<logout::Request>,
) -> ConduitResult<logout::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    db.users.remove_device(&sender_user, sender_device)?;

    db.flush()?;

    Ok(logout::Response::new().into())
}

/// # `POST /_matrix/client/r0/logout/all`
///
/// Log out all devices of this user.
///
/// - Invalidates all access tokens
/// - Deletes all device metadata (device id, device display name, last seen ip, last seen ts)
/// - Forgets all to-device events
/// - Triggers device list updates
///
/// Note: This is equivalent to calling [`GET /_matrix/client/r0/logout`](fn.logout_route.html)
/// from each device of this user.
#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/logout/all", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn logout_all_route(
    db: DatabaseGuard,
    body: Ruma<logout_all::Request>,
) -> ConduitResult<logout_all::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    for device_id in db.users.all_device_ids(sender_user).flatten() {
        db.users.remove_device(&sender_user, &device_id)?;
    }

    db.flush()?;

    Ok(logout_all::Response::new().into())
}
