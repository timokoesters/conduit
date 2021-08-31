use crate::ConduitResult;
use ruma::api::client::r0::thirdparty::get_protocols;

#[cfg(feature = "conduit_bin")]
use rocket::get;
use std::collections::BTreeMap;

/// # `GET /_matrix/client/r0/thirdparty/protocols`
///
/// TODO: Fetches all metadata about protocols supported by the homeserver.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/thirdparty/protocols")
)]
#[tracing::instrument]
pub async fn get_protocols_route() -> ConduitResult<get_protocols::Response> {
    // TODO
    Ok(get_protocols::Response {
        protocols: BTreeMap::new(),
    }
    .into())
}
