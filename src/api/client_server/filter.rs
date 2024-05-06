use crate::{services, Error, Result, Ruma};
use ruma::api::client::{
    error::ErrorKind,
    filter::{create_filter, get_filter},
};

/// # `GET /_matrix/client/r0/user/{userId}/filter/{filterId}`
///
/// Loads a filter that was previously created.
///
/// - A user can only access their own filters
pub async fn get_filter_route(
    body: Ruma<get_filter::v3::Request>,
) -> Result<get_filter::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let Some(filter) = services().users.get_filter(sender_user, &body.filter_id)? else {
        return Err(Error::BadRequest(ErrorKind::NotFound, "Filter not found."));
    };

    Ok(get_filter::v3::Response::new(filter))
}

/// # `PUT /_matrix/client/r0/user/{userId}/filter`
///
/// Creates a new filter to be used by other endpoints.
pub async fn create_filter_route(
    body: Ruma<create_filter::v3::Request>,
) -> Result<create_filter::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    Ok(create_filter::v3::Response::new(
        services().users.create_filter(sender_user, &body.filter)?,
    ))
}
