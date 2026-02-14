use ruma::api::client::relations::{
    get_relating_events, get_relating_events_with_rel_type,
    get_relating_events_with_rel_type_and_event_type,
};

use crate::{Result, Ruma, services};

/// # `GET /_matrix/client/r0/rooms/{roomId}/relations/{eventId}/{relType}/{eventType}`
pub async fn get_relating_events_with_rel_type_and_event_type_route(
    body: Ruma<get_relating_events_with_rel_type_and_event_type::v1::Request>,
) -> Result<get_relating_events_with_rel_type_and_event_type::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let res = services()
        .rooms
        .pdu_metadata
        .paginate_relations_with_filter(
            sender_user,
            &body.room_id,
            &body.event_id,
            Some(body.event_type.clone()),
            Some(body.rel_type.clone()),
            body.from.clone(),
            body.to.clone(),
            body.limit,
            body.recurse,
            &body.dir,
        )?;

    Ok(
        get_relating_events_with_rel_type_and_event_type::v1::Response {
            chunk: res.chunk,
            next_batch: res.next_batch,
            prev_batch: res.prev_batch,
            recursion_depth: res.recursion_depth,
        },
    )
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/relations/{eventId}/{relType}`
pub async fn get_relating_events_with_rel_type_route(
    body: Ruma<get_relating_events_with_rel_type::v1::Request>,
) -> Result<get_relating_events_with_rel_type::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let res = services()
        .rooms
        .pdu_metadata
        .paginate_relations_with_filter(
            sender_user,
            &body.room_id,
            &body.event_id,
            None,
            Some(body.rel_type.clone()),
            body.from.clone(),
            body.to.clone(),
            body.limit,
            body.recurse,
            &body.dir,
        )?;

    Ok(get_relating_events_with_rel_type::v1::Response {
        chunk: res.chunk,
        next_batch: res.next_batch,
        prev_batch: res.prev_batch,
        recursion_depth: res.recursion_depth,
    })
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/relations/{eventId}`
pub async fn get_relating_events_route(
    body: Ruma<get_relating_events::v1::Request>,
) -> Result<get_relating_events::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    services()
        .rooms
        .pdu_metadata
        .paginate_relations_with_filter(
            sender_user,
            &body.room_id,
            &body.event_id,
            None,
            None,
            body.from.clone(),
            body.to.clone(),
            body.limit,
            body.recurse,
            &body.dir,
        )
}
