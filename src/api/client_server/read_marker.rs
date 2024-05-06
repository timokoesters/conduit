use crate::{service::rooms::timeline::PduCount, services, Error, Result, Ruma};
use ruma::{
    api::client::{error::ErrorKind, read_marker::set_read_marker, receipt::create_receipt},
    events::{
        receipt::{ReceiptThread, ReceiptType},
        RoomAccountDataEventType,
    },
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
    body: Ruma<set_read_marker::v3::Request>,
) -> Result<set_read_marker::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if let Some(fully_read) = &body.fully_read {
        let fully_read_event = ruma::events::fully_read::FullyReadEvent {
            content: ruma::events::fully_read::FullyReadEventContent {
                event_id: fully_read.clone(),
            },
        };
        services().account_data.update(
            Some(&body.room_id),
            sender_user,
            RoomAccountDataEventType::FullyRead,
            &serde_json::to_value(fully_read_event).expect("to json value always works"),
        )?;
    }

    if body.private_read_receipt.is_some() || body.read_receipt.is_some() {
        services()
            .rooms
            .user
            .reset_notification_counts(sender_user, &body.room_id)?;
    }

    if let Some(event) = &body.private_read_receipt {
        let count = services()
            .rooms
            .timeline
            .get_pdu_count(event)?
            .ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event does not exist.",
            ))?;
        let count = match count {
            PduCount::Backfilled(_) => {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Read receipt is in backfilled timeline",
                ))
            }
            PduCount::Normal(c) => c,
        };
        services()
            .rooms
            .edus
            .read_receipt
            .private_read_set(&body.room_id, sender_user, count)?;
    }

    if let Some(event) = &body.read_receipt {
        let mut user_receipts = BTreeMap::new();
        user_receipts.insert(
            sender_user.clone(),
            ruma::events::receipt::Receipt {
                ts: Some(MilliSecondsSinceUnixEpoch::now()),
                thread: ReceiptThread::Unthreaded,
            },
        );

        let mut receipts = BTreeMap::new();
        receipts.insert(ReceiptType::Read, user_receipts);

        let mut receipt_content = BTreeMap::new();
        receipt_content.insert(event.to_owned(), receipts);

        services().rooms.edus.read_receipt.readreceipt_update(
            sender_user,
            &body.room_id,
            ruma::events::receipt::ReceiptEvent {
                content: ruma::events::receipt::ReceiptEventContent(receipt_content),
                room_id: body.room_id.clone(),
            },
        )?;
    }

    Ok(set_read_marker::v3::Response {})
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/receipt/{receiptType}/{eventId}`
///
/// Sets private read marker and public read receipt EDU.
pub async fn create_receipt_route(
    body: Ruma<create_receipt::v3::Request>,
) -> Result<create_receipt::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if matches!(
        &body.receipt_type,
        create_receipt::v3::ReceiptType::Read | create_receipt::v3::ReceiptType::ReadPrivate
    ) {
        services()
            .rooms
            .user
            .reset_notification_counts(sender_user, &body.room_id)?;
    }

    match body.receipt_type {
        create_receipt::v3::ReceiptType::FullyRead => {
            let fully_read_event = ruma::events::fully_read::FullyReadEvent {
                content: ruma::events::fully_read::FullyReadEventContent {
                    event_id: body.event_id.clone(),
                },
            };
            services().account_data.update(
                Some(&body.room_id),
                sender_user,
                RoomAccountDataEventType::FullyRead,
                &serde_json::to_value(fully_read_event).expect("to json value always works"),
            )?;
        }
        create_receipt::v3::ReceiptType::Read => {
            let mut user_receipts = BTreeMap::new();
            user_receipts.insert(
                sender_user.clone(),
                ruma::events::receipt::Receipt {
                    ts: Some(MilliSecondsSinceUnixEpoch::now()),
                    thread: ReceiptThread::Unthreaded,
                },
            );
            let mut receipts = BTreeMap::new();
            receipts.insert(ReceiptType::Read, user_receipts);

            let mut receipt_content = BTreeMap::new();
            receipt_content.insert(body.event_id.clone(), receipts);

            services().rooms.edus.read_receipt.readreceipt_update(
                sender_user,
                &body.room_id,
                ruma::events::receipt::ReceiptEvent {
                    content: ruma::events::receipt::ReceiptEventContent(receipt_content),
                    room_id: body.room_id.clone(),
                },
            )?;
        }
        create_receipt::v3::ReceiptType::ReadPrivate => {
            let count = services()
                .rooms
                .timeline
                .get_pdu_count(&body.event_id)?
                .ok_or(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Event does not exist.",
                ))?;
            let count = match count {
                PduCount::Backfilled(_) => {
                    return Err(Error::BadRequest(
                        ErrorKind::InvalidParam,
                        "Read receipt is in backfilled timeline",
                    ))
                }
                PduCount::Normal(c) => c,
            };
            services().rooms.edus.read_receipt.private_read_set(
                &body.room_id,
                sender_user,
                count,
            )?;
        }
        _ => return Err(Error::bad_database("Unsupported receipt type")),
    }

    Ok(create_receipt::v3::Response {})
}
