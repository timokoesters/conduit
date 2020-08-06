use crate::{utils, ConduitResult};
use ruma::api::client::r0::filter::{self, create_filter, get_filter};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(feature = "conduit_bin", get("/_matrix/client/r0/user/<_>/filter/<_>"))]
pub fn get_filter_route() -> ConduitResult<get_filter::Response> {
    // TODO
    Ok(get_filter::Response::new(filter::FilterDefinition {
        event_fields: None,
        event_format: None,
        account_data: None,
        room: None,
        presence: None,
    })
    .into())
}

#[cfg_attr(feature = "conduit_bin", post("/_matrix/client/r0/user/<_>/filter"))]
pub fn create_filter_route() -> ConduitResult<create_filter::Response> {
    // TODO
    Ok(create_filter::Response::new(utils::random_string(10)).into())
}
