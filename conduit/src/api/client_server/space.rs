use std::str::FromStr;

use crate::{Error, Result, Ruma, service::rooms::spaces::PagnationToken, services};
use ruma::{
    UInt,
    api::client::{error::ErrorKind, space::get_hierarchy},
};

/// # `GET /_matrix/client/v1/rooms/{room_id}/hierarchy``
///
/// Paginates over the space tree in a depth-first manner to locate child rooms of a given space.
pub async fn get_hierarchy_route(
    body: Ruma<get_hierarchy::v1::Request>,
) -> Result<get_hierarchy::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let limit = body
        .limit
        .unwrap_or(UInt::from(10_u32))
        .min(UInt::from(100_u32));
    let max_depth = body
        .max_depth
        .unwrap_or(UInt::from(3_u32))
        .min(UInt::from(10_u32));

    let key = body
        .from
        .as_ref()
        .and_then(|s| PagnationToken::from_str(s).ok());

    // Should prevent unexpected behaviour in (bad) clients
    if let Some(token) = &key {
        if token.suggested_only != body.suggested_only || token.max_depth != max_depth {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "suggested_only and max_depth cannot change on paginated requests",
            ));
        }
    }

    services()
        .rooms
        .spaces
        .get_client_hierarchy(
            sender_user,
            &body.room_id,
            usize::try_from(limit)
                .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Limit is too great"))?,
            key.map_or(vec![], |token| token.short_room_ids),
            usize::try_from(max_depth).map_err(|_| {
                Error::BadRequest(ErrorKind::InvalidParam, "Max depth is too great")
            })?,
            body.suggested_only,
        )
        .await
}
