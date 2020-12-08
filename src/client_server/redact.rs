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
pub async fn redact_event_route(
    db: State<'_, Database>,
    body: Ruma<redact_event::Request<'_>>,
) -> ConduitResult<redact_event::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event_id = db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomRedaction,
            content: serde_json::to_value(redaction::RedactionEventContent {
                reason: body.reason.clone(),
            })
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: None,
            redacts: Some(body.event_id.clone()),
        },
        &sender_user,
        &body.room_id,
        &db.globals,
        &db.sending,
        &db.admin,
        &db.account_data,
        &db.appservice,
    )?;

    db.flush().await?;

    Ok(redact_event::Response { event_id }.into())
}
