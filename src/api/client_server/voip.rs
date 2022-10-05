use crate::{Result, Ruma, services};
use hmac::{Hmac, Mac, NewMac};
use ruma::{api::client::voip::get_turn_server_info, SecondsSinceUnixEpoch};
use sha1::Sha1;
use std::time::{Duration, SystemTime};

type HmacSha1 = Hmac<Sha1>;

/// # `GET /_matrix/client/r0/voip/turnServer`
///
/// TODO: Returns information about the recommended turn server.
pub async fn turn_server_route(
    body: Ruma<get_turn_server_info::v3::IncomingRequest>,
) -> Result<get_turn_server_info::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let turn_secret = services().globals.turn_secret().clone();

    let (username, password) = if !turn_secret.is_empty() {
        let expiry = SecondsSinceUnixEpoch::from_system_time(
            SystemTime::now() + Duration::from_secs(services().globals.turn_ttl()),
        )
        .expect("time is valid");

        let username: String = format!("{}:{}", expiry.get(), sender_user);

        let mut mac = HmacSha1::new_from_slice(turn_secret.as_bytes())
            .expect("HMAC can take key of any size");
        mac.update(username.as_bytes());

        let password: String = base64::encode_config(mac.finalize().into_bytes(), base64::STANDARD);

        (username, password)
    } else {
        (
            services().globals.turn_username().clone(),
            services().globals.turn_password().clone(),
        )
    };

    Ok(get_turn_server_info::v3::Response {
        username,
        password,
        uris: services().globals.turn_uris().to_vec(),
        ttl: Duration::from_secs(services().globals.turn_ttl()),
    })
}
