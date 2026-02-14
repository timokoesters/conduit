use std::sync::Arc;

use crate::{Result, Ruma, service::pdu::PduBuilder, services};
use ruma::{
    api::client::redact::redact_event,
    events::{TimelineEventType, room::redaction::RoomRedactionEventContent},
};

use serde_json::value::to_raw_value;

/// # `PUT /_matrix/client/r0/rooms/{roomId}/redact/{eventId}/{txnId}`
///
/// Tries to send a redaction event into the room.
///
/// - TODO: Handle txn id
pub async fn redact_event_route(
    body: Ruma<redact_event::v3::Request>,
) -> Result<redact_event::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let body = body.body;

    let mutex_state = Arc::clone(
        services()
            .globals
            .roomid_mutex_state
            .write()
            .await
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    let event_id = services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomRedaction,
                content: to_raw_value(&RoomRedactionEventContent {
                    redacts: Some(body.event_id.clone()),
                    reason: body.reason.clone(),
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: None,
                redacts: Some(body.event_id.into()),
                timestamp: None,
            },
            sender_user,
            &body.room_id,
            &state_lock,
        )
        .await?;

    drop(state_lock);

    let event_id = (*event_id).to_owned();
    Ok(redact_event::v3::Response { event_id })
}
