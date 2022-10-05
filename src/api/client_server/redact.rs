use std::sync::Arc;

use crate::{Result, Ruma, services, service::pdu::PduBuilder};
use ruma::{
    api::client::redact::redact_event,
    events::{room::redaction::RoomRedactionEventContent, RoomEventType},
};

use serde_json::value::to_raw_value;

/// # `PUT /_matrix/client/r0/rooms/{roomId}/redact/{eventId}/{txnId}`
///
/// Tries to send a redaction event into the room.
///
/// - TODO: Handle txn id
pub async fn redact_event_route(
    body: Ruma<redact_event::v3::IncomingRequest>,
) -> Result<redact_event::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let body = body.body;

    let mutex_state = Arc::clone(
        services().globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    let event_id = services().rooms.timeline.build_and_append_pdu(
        PduBuilder {
            event_type: RoomEventType::RoomRedaction,
            content: to_raw_value(&RoomRedactionEventContent {
                reason: body.reason.clone(),
            })
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: None,
            redacts: Some(body.event_id.into()),
        },
        sender_user,
        &body.room_id,
        &state_lock,
    )?;

    drop(state_lock);

    let event_id = (*event_id).to_owned();
    Ok(redact_event::v3::Response { event_id })
}
