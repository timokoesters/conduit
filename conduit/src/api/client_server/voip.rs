use crate::{Error, Result, Ruma, config::TurnAuth, services};
use base64::{Engine as _, engine::general_purpose};
use hmac::{Hmac, Mac};
use ruma::{
    SecondsSinceUnixEpoch,
    api::client::{error::ErrorKind, voip::get_turn_server_info},
};
use sha1::Sha1;
use std::time::{Duration, SystemTime};

type HmacSha1 = Hmac<Sha1>;

/// # `GET /_matrix/client/r0/voip/turnServer`
///
/// Returns information about the recommended turn server.
pub async fn turn_server_route(
    body: Ruma<get_turn_server_info::v3::Request>,
) -> Result<get_turn_server_info::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if let Some(turn) = services().globals.turn() {
        let (username, password) = match turn.auth {
            TurnAuth::Secret { secret } => {
                let expiry = SecondsSinceUnixEpoch::from_system_time(
                    SystemTime::now() + Duration::from_secs(turn.ttl),
                )
                .expect("time is valid");

                let username: String = format!("{}:{}", expiry.get(), sender_user);

                let mut mac = HmacSha1::new_from_slice(secret.as_bytes())
                    .expect("HMAC can take key of any size");
                mac.update(username.as_bytes());

                let password: String =
                    general_purpose::STANDARD.encode(mac.finalize().into_bytes());

                (username, password)
            }
            TurnAuth::UserPass { username, password } => (username, password),
        };

        Ok(get_turn_server_info::v3::Response {
            username,
            password,
            uris: turn.uris,
            ttl: Duration::from_secs(turn.ttl),
        })
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "No TURN config set"))
    }
}
