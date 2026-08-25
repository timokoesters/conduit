use std::time::Duration;

use ruma::{
    api::client::{account, error::ErrorKind},
    authentication::TokenType,
};

use crate::{Error, Result, Ruma, services};

/// # `POST /_matrix/client/r0/user/{userId}/openid/request_token`
///
/// Request an OpenID token to verify identity with third-party services.
///
/// - The token generated is only valid for the OpenID API.
pub async fn create_openid_token_route(
    body: Ruma<account::request_openid_token::v3::Request>,
) -> Result<account::request_openid_token::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if &body.user_id != sender_user {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "requested user ID does not match sender",
        ));
    }

    let (access_token, expires_in) = services().users.create_openid_token(&body.user_id)?;

    Ok(account::request_openid_token::v3::Response {
        access_token,
        token_type: TokenType::Bearer,
        matrix_server_name: services().globals.server_name().to_owned(),
        expires_in: Duration::from_secs(expires_in),
    })
}
