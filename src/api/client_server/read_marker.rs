use crate::{database::DatabaseGuard, Error, Result, Ruma};
use ruma::{
    api::client::{error::ErrorKind, read_marker::set_read_marker, receipt::create_receipt},
    events::RoomAccountDataEventType,
    receipt::ReceiptType,
    MilliSecondsSinceUnixEpoch,
};
use std::collections::BTreeMap;

/// # `POST /_matrix/client/r0/rooms/{roomId}/read_markers`
///
/// Sets different types of read markers.
///
/// - Updates fully-read account data event to `fully_read`
/// - If `read_receipt` is set: Update private marker and public read receipt EDU
pub async fn set_read_marker_route(
    db: DatabaseGuard,
    body: Ruma<set_read_marker::v3::IncomingRequest>,
) -> Result<set_read_marker::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let fully_read_event = ruma::events::fully_read::FullyReadEvent {
        content: ruma::events::fully_read::FullyReadEventContent {
            event_id: body.fully_read.clone(),
        },
    };
    db.account_data.update(
        Some(&body.room_id),
        sender_user,
        RoomAccountDataEventType::FullyRead,
        &fully_read_event,
        &db.globals,
    )?;

    if let Some(event) = &body.read_receipt {
        db.rooms.edus.private_read_set(
            &body.room_id,
            sender_user,
            db.rooms.get_pdu_count(event)?.ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event does not exist.",
            ))?,
            &db.globals,
        )?;
        db.rooms
            .reset_notification_counts(sender_user, &body.room_id)?;

        let mut user_receipts = BTreeMap::new();
        user_receipts.insert(
            sender_user.clone(),
            ruma::events::receipt::Receipt {
                ts: Some(MilliSecondsSinceUnixEpoch::now()),
            },
        );

        let mut receipts = BTreeMap::new();
        receipts.insert(ReceiptType::Read, user_receipts);

        let mut receipt_content = BTreeMap::new();
        receipt_content.insert(event.to_owned(), receipts);

        db.rooms.edus.readreceipt_update(
            sender_user,
            &body.room_id,
            ruma::events::receipt::ReceiptEvent {
                content: ruma::events::receipt::ReceiptEventContent(receipt_content),
                room_id: body.room_id.clone(),
            },
            &db.globals,
        )?;
    }

    db.flush()?;

    Ok(set_read_marker::v3::Response {})
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/receipt/{receiptType}/{eventId}`
///
/// Sets private read marker and public read receipt EDU.
pub async fn create_receipt_route(
    db: DatabaseGuard,
    body: Ruma<create_receipt::v3::IncomingRequest>,
) -> Result<create_receipt::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.rooms.edus.private_read_set(
        &body.room_id,
        sender_user,
        db.rooms
            .get_pdu_count(&body.event_id)?
            .ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event does not exist.",
            ))?,
        &db.globals,
    )?;
    db.rooms
        .reset_notification_counts(sender_user, &body.room_id)?;

    let mut user_receipts = BTreeMap::new();
    user_receipts.insert(
        sender_user.clone(),
        ruma::events::receipt::Receipt {
            ts: Some(MilliSecondsSinceUnixEpoch::now()),
        },
    );
    let mut receipts = BTreeMap::new();
    receipts.insert(ReceiptType::Read, user_receipts);

    let mut receipt_content = BTreeMap::new();
    receipt_content.insert(body.event_id.to_owned(), receipts);

    db.rooms.edus.readreceipt_update(
        sender_user,
        &body.room_id,
        ruma::events::receipt::ReceiptEvent {
            content: ruma::events::receipt::ReceiptEventContent(receipt_content),
            room_id: body.room_id.clone(),
        },
        &db.globals,
    )?;

    db.flush()?;

    Ok(create_receipt::v3::Response {})
}
