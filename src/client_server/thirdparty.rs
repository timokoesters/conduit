use crate::ConduitResult;
use ruma::api::client::r0::thirdparty::get_protocols;

use log::warn;
#[cfg(feature = "conduit_bin")]
use rocket::get;
use std::collections::BTreeMap;

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/thirdparty/protocols")
)]
#[tracing::instrument]
pub async fn get_protocols_route() -> ConduitResult<get_protocols::Response> {
    warn!("TODO: get_protocols_route");
    Ok(get_protocols::Response {
        protocols: BTreeMap::new(),
    }
    .into())
}
