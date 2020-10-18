use crate::{utils, ConduitResult};
use ruma::api::client::r0::filter::{create_filter, get_filter, IncomingFilterDefinition};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(feature = "conduit_bin", get("/_matrix/client/r0/user/<_>/filter/<_>"))]
pub async fn get_filter_route() -> ConduitResult<get_filter::Response> {
    // TODO
    Ok(get_filter::Response::new(IncomingFilterDefinition::default()).into())
}

#[cfg_attr(feature = "conduit_bin", post("/_matrix/client/r0/user/<_>/filter"))]
pub async fn create_filter_route() -> ConduitResult<create_filter::Response> {
    // TODO
    Ok(create_filter::Response::new(utils::random_string(10)).into())
}
