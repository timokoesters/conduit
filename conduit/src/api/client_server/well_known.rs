use ruma::api::client::discovery::{
    discover_homeserver::{self, HomeserverInfo},
    discover_support,
};

use crate::{Result, Ruma, services};

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
    })
}

/// # `GET /.well-known/matrix/support`
///
/// Returns server admin contact and support page of the domain.
pub async fn well_known_support(
    _body: Ruma<discover_support::Request>,
) -> Result<discover_support::Response> {
    let conduit_config::WellKnownSupportConfig {
        mut contacts,
        publish_admins,
        page,
    } = services().globals.config.well_known.support.clone();

    if publish_admins {
        contacts.append(
            &mut services()
                .globals
                .config
                .admins
                .clone()
                .into_iter()
                .map(Into::into)
                .collect(),
        )
    }

    Ok(discover_support::Response {
        contacts,
        support_page: page.map(|page| page.to_string()),
    })
}
