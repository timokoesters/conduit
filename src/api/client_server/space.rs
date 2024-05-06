use crate::{services, Result, Ruma};
use ruma::api::client::space::get_hierarchy;

/// # `GET /_matrix/client/v1/rooms/{room_id}/hierarchy`
///
/// Paginates over the space tree in a depth-first manner to locate child rooms of a given space.
pub async fn get_hierarchy_route(
    body: Ruma<get_hierarchy::v1::Request>,
) -> Result<get_hierarchy::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let skip = body
        .from
        .as_ref()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);

    let limit = body.limit.map_or(10, u64::from).min(100) as usize;

    let max_depth = body.max_depth.map_or(3, u64::from).min(10) as usize + 1; // +1 to skip the space room itself

    services()
        .rooms
        .spaces
        .get_hierarchy(
            sender_user,
            &body.room_id,
            limit,
            skip,
            max_depth,
            body.suggested_only,
        )
        .await
}
