use std::{collections::BTreeMap, iter::FromIterator};

use axum::{response::IntoResponse, Json};
use ruma::api::client::{discovery::get_supported_versions, error::ErrorKind};

use crate::{services, Error, Result, Ruma};

/// # `GET /_matrix/client/versions`
///
/// Get the versions of the specification and unstable features supported by this server.
///
/// - Versions take the form MAJOR.MINOR.PATCH
/// - Only the latest PATCH release will be reported for each MAJOR.MINOR value
/// - Unstable features are namespaced and may include version information in their name
///
/// Note: Unstable features are used while developing new features. Clients should avoid using
/// unstable features in their stable releases
pub async fn get_supported_versions_route(
    _body: Ruma<get_supported_versions::Request>,
) -> Result<get_supported_versions::Response> {
    let resp = get_supported_versions::Response {
        versions: vec![
            "r0.5.0".to_owned(),
            "r0.6.0".to_owned(),
            "v1.1".to_owned(),
            "v1.2".to_owned(),
            "v1.3".to_owned(),
            "v1.4".to_owned(),
            "v1.5".to_owned(),
        ],
        unstable_features: BTreeMap::from_iter([("org.matrix.e2e_cross_signing".to_owned(), true)]),
    };

    Ok(resp)
}

/// # `GET /.well-known/matrix/client`
pub async fn well_known_client_route(
    _body: Ruma<get_supported_versions::Request>,
) -> Result<impl IntoResponse> {
    let client_url = match services().globals.well_known_client() {
        Some(url) => url.clone(),
        None => return Err(Error::BadRequest(ErrorKind::NotFound, "Not found.")),
    };

    Ok(Json(serde_json::json!({
        "m.homeserver": {"base_url": client_url},
        "org.matrix.msc3575.proxy": {"url": client_url}
    })))
}
