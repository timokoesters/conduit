use ruma::api::client::{error::ErrorKind, threads::get_threads};

use crate::{services, Error, Result, Ruma};

/// # `GET /_matrix/client/r0/rooms/{roomId}/threads`
pub async fn get_threads_route(
    body: Ruma<get_threads::v1::Request>,
) -> Result<get_threads::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    // Use limit or else 10, with maximum 100
    let limit = body
        .limit
        .and_then(|l| l.try_into().ok())
        .unwrap_or(10)
        .min(100);

    let from = if let Some(from) = &body.from {
        from.parse()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, ""))?
    } else {
        u64::MAX
    };

    let threads = services()
        .rooms
        .threads
        .threads_until(sender_user, &body.room_id, from, &body.include)?
        .take(limit)
        .filter_map(|r| r.ok())
        .filter(|(_, pdu)| {
            services()
                .rooms
                .state_accessor
                .user_can_see_event(sender_user, &body.room_id, &pdu.event_id)
                .unwrap_or(false)
        })
        .collect::<Vec<_>>();

    let next_batch = threads.last().map(|(count, _)| count.to_string());

    Ok(get_threads::v1::Response {
        chunk: threads
            .into_iter()
            .map(|(_, pdu)| pdu.to_room_event())
            .collect(),
        next_batch,
    })
}
