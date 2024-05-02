use std::{collections::BTreeMap, iter::FromIterator};

use ruma::api::client::discovery::get_supported_versions;

use crate::{Result, Ruma};

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
