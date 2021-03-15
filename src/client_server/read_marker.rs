use super::State;
use crate::{ConduitResult, Database, Error, Ruma};
use ruma::{api::client::{error::ErrorKind, r0::{capabilities::get_capabilities, read_marker::set_read_marker, receipt::create_receipt}}, events::{AnyEphemeralRoomEvent, AnyEvent, EventType}};

#[cfg(feature = "conduit_bin")]
use rocket::post;
use std::{collections::BTreeMap, time::SystemTime};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/read_markers", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn set_read_marker_route(
    db: State<'_, Database>,
    body: Ruma<set_read_marker::Request<'_>>,
) -> ConduitResult<set_read_marker::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let fully_read_event = ruma::events::fully_read::FullyReadEvent {
        content: ruma::events::fully_read::FullyReadEventContent {
            event_id: body.fully_read.clone(),
        },
        room_id: body.room_id.clone(),
    };
    db.account_data.update(
        Some(&body.room_id),
        &sender_user,
        EventType::FullyRead,
        &fully_read_event,
        &db.globals,
    )?;

    if let Some(event) = &body.read_receipt {
        db.rooms.edus.private_read_set(
            &body.room_id,
            &sender_user,
            db.rooms.get_pdu_count(event)?.ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event does not exist.",
            ))?,
            &db.globals,
        )?;

        let mut user_receipts = BTreeMap::new();
        user_receipts.insert(
            sender_user.clone(),
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
            &sender_user,
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

    db.flush().await?;

    Ok(set_read_marker::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/receipt/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn create_receipt_route(
    db: State<'_, Database>,
    body: Ruma<create_receipt::Request<'_>>,
) -> ConduitResult<create_receipt::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.rooms.edus.private_read_set(
        &body.room_id,
        &sender_user,
        db.rooms
            .get_pdu_count(&body.event_id)?
            .ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event does not exist.",
            ))?,
        &db.globals,
    )?;

    let mut user_receipts = BTreeMap::new();
    user_receipts.insert(
        sender_user.clone(),
        ruma::events::receipt::Receipt {
            ts: Some(SystemTime::now()),
        },
    );
    let mut receipt_content = BTreeMap::new();
    receipt_content.insert(
        body.event_id.to_owned(),
        ruma::events::receipt::Receipts {
            read: Some(user_receipts),
        },
    );

    db.rooms.edus.readreceipt_update(
        &sender_user,
        &body.room_id,
        AnyEvent::Ephemeral(AnyEphemeralRoomEvent::Receipt(
            ruma::events::receipt::ReceiptEvent {
                content: ruma::events::receipt::ReceiptEventContent(receipt_content),
                room_id: body.room_id.clone(),
            },
        )),
        &db.globals,
    )?;

    db.flush().await?;

    Ok(create_receipt::Response.into())
}
