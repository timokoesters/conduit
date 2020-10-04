use super::State;
use crate::{ConduitResult, Database, Error, Ruma};
use ruma::{
    api::client::{error::ErrorKind, r0::read_marker::set_read_marker},
    events::{AnyEphemeralRoomEvent, AnyEvent, EventType},
};

#[cfg(feature = "conduit_bin")]
use rocket::post;
use std::{collections::BTreeMap, time::SystemTime};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/read_markers", data = "<body>")
)]
pub fn set_read_marker_route(
    db: State<'_, Database<'_>>,
    body: Ruma<set_read_marker::Request<'_>>,
) -> ConduitResult<set_read_marker::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    let fully_read_event = ruma::events::fully_read::FullyReadEvent {
        content: ruma::events::fully_read::FullyReadEventContent {
            event_id: body.fully_read.clone(),
        },
        room_id: body.room_id.clone(),
    };
    db.account_data.update(
        Some(&body.room_id),
        &sender_id,
        EventType::FullyRead,
        &fully_read_event,
        &db.globals,
    )?;

    if let Some(event) = &body.read_receipt {
        db.rooms.edus.private_read_set(
            &body.room_id,
            &sender_id,
            db.rooms.get_pdu_count(event)?.ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event does not exist.",
            ))?,
            &db.globals,
        )?;

        let mut user_receipts = BTreeMap::new();
        user_receipts.insert(
            sender_id.clone(),
            ruma::events::receipt::Receipt {
                ts: Some(SystemTime::now()),
            },
        );
        let mut receipt_content = BTreeMap::new();
        receipt_content.insert(
            event.to_owned(),
            ruma::events::receipt::Receipts {
                read: Some(user_receipts),
            },
        );

        db.rooms.edus.readreceipt_update(
            &sender_id,
            &body.room_id,
            AnyEvent::Ephemeral(AnyEphemeralRoomEvent::Receipt(
                ruma::events::receipt::ReceiptEvent {
                    content: ruma::events::receipt::ReceiptEventContent(receipt_content),
                    room_id: body.room_id.clone(),
                },
            )),
            &db.globals,
        )?;
    }
    Ok(set_read_marker::Response.into())
}
