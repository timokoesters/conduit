use std::time::Duration;

use ruma::{api::client::account, authentication::TokenType};

use crate::{services, Result, Ruma};

/// # `POST /_matrix/client/r0/user/{userId}/openid/request_token`
///
/// Request an OpenID token to verify identity with third-party services.
///
/// - The token generated is only valid for the OpenID API.
pub async fn create_openid_token_route(
    body: Ruma<account::request_openid_token::v3::Request>,
) -> Result<account::request_openid_token::v3::Response> {
    let (access_token, expires_in) = services().users.create_openid_token(&body.user_id)?;

    Ok(account::request_openid_token::v3::Response {
        access_token,
        token_type: TokenType::Bearer,
        matrix_server_name: services().globals.server_name().to_owned(),
        expires_in: Duration::from_secs(expires_in),
    })
}
