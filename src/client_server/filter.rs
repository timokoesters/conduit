use crate::{utils, ConduitResult};
use ruma::api::client::r0::filter::{self, create_filter, get_filter};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

/// # `GET /_matrix/client/r0/user/{userId}/filter/{filterId}`
///
/// TODO: Loads a filter that was previously created.
#[cfg_attr(feature = "conduit_bin", get("/_matrix/client/r0/user/<_>/filter/<_>"))]
#[tracing::instrument]
pub async fn get_filter_route() -> ConduitResult<get_filter::Response> {
    // TODO
    Ok(get_filter::Response::new(filter::IncomingFilterDefinition {
        event_fields: None,
        event_format: filter::EventFormat::default(),
        account_data: filter::IncomingFilter::default(),
        room: filter::IncomingRoomFilter::default(),
        presence: filter::IncomingFilter::default(),
    })
    .into())
}

/// # `PUT /_matrix/client/r0/user/{userId}/filter`
///
/// TODO: Creates a new filter to be used by other endpoints.
#[cfg_attr(feature = "conduit_bin", post("/_matrix/client/r0/user/<_>/filter"))]
#[tracing::instrument]
pub async fn create_filter_route() -> ConduitResult<create_filter::Response> {
    // TODO
    Ok(create_filter::Response::new(utils::random_string(10)).into())
}
