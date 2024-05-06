use ruma::api::client::relations::{
    get_relating_events, get_relating_events_with_rel_type,
    get_relating_events_with_rel_type_and_event_type,
};

use crate::{service::rooms::timeline::PduCount, services, Result, Ruma};

/// # `GET /_matrix/client/r0/rooms/{roomId}/relations/{eventId}/{relType}/{eventType}`
pub async fn get_relating_events_with_rel_type_and_event_type_route(
    body: Ruma<get_relating_events_with_rel_type_and_event_type::v1::Request>,
) -> Result<get_relating_events_with_rel_type_and_event_type::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let from = match body.from.clone() {
        Some(from) => PduCount::try_from_string(&from)?,
        None => match ruma::api::Direction::Backward {
            // TODO: fix ruma so `body.dir` exists
            ruma::api::Direction::Forward => PduCount::min(),
            ruma::api::Direction::Backward => PduCount::max(),
        },
    };

    let to = body
        .to
        .as_ref()
        .and_then(|t| PduCount::try_from_string(t).ok());

    // Use limit or else 10, with maximum 100
    let limit = body
        .limit
        .and_then(|u| u32::try_from(u).ok())
        .map_or(10_usize, |u| u as usize)
        .min(100);

    let res = services()
        .rooms
        .pdu_metadata
        .paginate_relations_with_filter(
            sender_user,
            &body.room_id,
            &body.event_id,
            &Some(body.event_type.clone()),
            &Some(body.rel_type.clone()),
            from,
            to,
            limit,
        )?;

    Ok(
        get_relating_events_with_rel_type_and_event_type::v1::Response {
            chunk: res.chunk,
            next_batch: res.next_batch,
            prev_batch: res.prev_batch,
        },
    )
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/relations/{eventId}/{relType}`
pub async fn get_relating_events_with_rel_type_route(
    body: Ruma<get_relating_events_with_rel_type::v1::Request>,
) -> Result<get_relating_events_with_rel_type::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let from = match body.from.clone() {
        Some(from) => PduCount::try_from_string(&from)?,
        None => match ruma::api::Direction::Backward {
            // TODO: fix ruma so `body.dir` exists
            ruma::api::Direction::Forward => PduCount::min(),
            ruma::api::Direction::Backward => PduCount::max(),
        },
    };

    let to = body
        .to
        .as_ref()
        .and_then(|t| PduCount::try_from_string(t).ok());

    // Use limit or else 10, with maximum 100
    let limit = body
        .limit
        .and_then(|u| u32::try_from(u).ok())
        .map_or(10_usize, |u| u as usize)
        .min(100);

    let res = services()
        .rooms
        .pdu_metadata
        .paginate_relations_with_filter(
            sender_user,
            &body.room_id,
            &body.event_id,
            &None,
            &Some(body.rel_type.clone()),
            from,
            to,
            limit,
        )?;

    Ok(get_relating_events_with_rel_type::v1::Response {
        chunk: res.chunk,
        next_batch: res.next_batch,
        prev_batch: res.prev_batch,
    })
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/relations/{eventId}`
pub async fn get_relating_events_route(
    body: Ruma<get_relating_events::v1::Request>,
) -> Result<get_relating_events::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let from = match body.from.clone() {
        Some(from) => PduCount::try_from_string(&from)?,
        None => match ruma::api::Direction::Backward {
            // TODO: fix ruma so `body.dir` exists
            ruma::api::Direction::Forward => PduCount::min(),
            ruma::api::Direction::Backward => PduCount::max(),
        },
    };

    let to = body
        .to
        .as_ref()
        .and_then(|t| PduCount::try_from_string(t).ok());

    // Use limit or else 10, with maximum 100
    let limit = body
        .limit
        .and_then(|u| u32::try_from(u).ok())
        .map_or(10_usize, |u| u as usize)
        .min(100);

    services()
        .rooms
        .pdu_metadata
        .paginate_relations_with_filter(
            sender_user,
            &body.room_id,
            &body.event_id,
            &None,
            &None,
            from,
            to,
            limit,
        )
}
