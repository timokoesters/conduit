use crate::ConduitResult;
use ruma::api::client::unversioned::get_supported_versions;

#[cfg(feature = "conduit_bin")]
use rocket::get;

/// # `GET /_matrix/client/versions`
///
/// Get the versions of the specification and unstable features supported by this server.
///
/// - Versions take the form MAJOR.MINOR.PATCH
/// - Only the latest PATCH release will be reported for each MAJOR.MINOR value
/// - Unstable features should be namespaced and may include version information in their name
///
/// Note: Unstable features are used while developing new features. Clients should avoid using
/// unstable features in their stable releases
#[cfg_attr(feature = "conduit_bin", get("/_matrix/client/versions"))]
pub fn get_supported_versions_route() -> ConduitResult<get_supported_versions::Response> {
    let mut resp =
        get_supported_versions::Response::new(vec!["r0.5.0".to_owned(), "r0.6.0".to_owned()]);

    resp.unstable_features
        .insert("org.matrix.e2e_cross_signing".to_owned(), true);

    Ok(resp.into())
}
