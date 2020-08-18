use super::State;
use crate::{pdu::PduBuilder, ConduitResult, Database, Ruma};
use ruma::{
    api::client::r0::redact::redact_event,
    events::{room::redaction, EventType},
};

#[cfg(feature = "conduit_bin")]
use rocket::put;

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/rooms/<_>/redact/<_>/<_>", data = "<body>")
)]
pub fn redact_event_route(
    db: State<'_, Database>,
    body: Ruma<redact_event::Request>,
) -> ConduitResult<redact_event::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    let event_id = db.rooms.build_and_append_pdu(
        PduBuilder {
            room_id: body.room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomRedaction,
            content: serde_json::to_value(redaction::RedactionEventContent {
                reason: body.reason.clone(),
            })
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: None,
            redacts: Some(body.event_id.clone()),
        },
        &db.globals,
        &db.account_data,
    )?;

    Ok(redact_event::Response { event_id }.into())
}
