use ruma::api::client::discovery::discover_homeserver::{
    self, HomeserverInfo, SlidingSyncProxyInfo,
};

use crate::{services, Result, Ruma};

/// # `GET /.well-known/matrix/client`
///
/// Returns the client server discovery information.
pub async fn well_known_client(
    _body: Ruma<discover_homeserver::Request>,
) -> Result<discover_homeserver::Response> {
    let client_url = services().globals.well_known_client();

    Ok(discover_homeserver::Response {
        homeserver: HomeserverInfo {
            base_url: client_url.clone(),
        },
        identity_server: None,
        sliding_sync_proxy: Some(SlidingSyncProxyInfo { url: client_url }),
    })
}
