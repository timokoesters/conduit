use crate::{database::DatabaseGuard, pdu::PduBuilder, utils, ConduitResult, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::message::{get_message_events, send_message_event},
    },
    events::EventType,
    EventId,
};
use std::{
    collections::BTreeMap,
    convert::{TryFrom, TryInto},
    sync::Arc,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, put};

/// # `PUT /_matrix/client/r0/rooms/{roomId}/send/{eventType}/{txnId}`
///
/// Send a message event into the room.
///
/// - Is a NOOP if the txn id was already used before and returns the same event id again
/// - The only requirement for the content is that it has to be valid json
/// - Tries to send the event into the room, auth rules will determine if it is allowed
#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/rooms/<_>/send/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn send_message_event_route(
    db: DatabaseGuard,
    body: Ruma<send_message_event::Request<'_>>,
) -> ConduitResult<send_message_event::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_deref();

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    // Check if this is a new transaction id
    if let Some(response) =
        db.transaction_ids
            .existing_txnid(sender_user, sender_device, &body.txn_id)?
    {
        // The client might have sent a txnid of the /sendToDevice endpoint
        // This txnid has no response associated with it
        if response.is_empty() {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Tried to use txn id already used for an incompatible endpoint.",
            ));
        }

        let event_id = EventId::try_from(
            utils::string_from_bytes(&response)
                .map_err(|_| Error::bad_database("Invalid txnid bytes in database."))?,
        )
        .map_err(|_| Error::bad_database("Invalid event id in txnid data."))?;
        return Ok(send_message_event::Response { event_id }.into());
    }

    let mut unsigned = BTreeMap::new();
    unsigned.insert("transaction_id".to_owned(), body.txn_id.clone().into());

    let event_id = db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::from(&body.event_type),
            content: serde_json::from_str(body.body.body.json().get())
                .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?,
            unsigned: Some(unsigned),
            state_key: None,
            redacts: None,
        },
        sender_user,
        &body.room_id,
        &db,
        &state_lock,
    )?;

    db.transaction_ids.add_txnid(
        sender_user,
        sender_device,
        &body.txn_id,
        event_id.as_bytes(),
    )?;

    drop(state_lock);

    db.flush()?;

    Ok(send_message_event::Response::new(event_id).into())
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/messages`
///
/// Allows paginating through room history.
///
/// - Only works if the user is joined (TODO: always allow, but only show events where the user was
/// joined, depending on history_visibility)
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/messages", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_message_events_route(
    db: DatabaseGuard,
    body: Ruma<get_message_events::Request<'_>>,
) -> ConduitResult<get_message_events::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(sender_user, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    let from = body
        .from
        .clone()
        .parse()
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid `from` value."))?;

    let to = body.to.as_ref().map(|t| t.parse());

    // Use limit or else 10
    let limit = body
        .limit
        .try_into()
        .map_or(Ok::<_, Error>(10_usize), |l: u32| Ok(l as usize))?;

    match body.dir {
        get_message_events::Direction::Forward => {
            let events_after = db
                .rooms
                .pdus_after(sender_user, &body.room_id, from)?
                .take(limit)
                .filter_map(|r| r.ok()) // Filter out buggy events
                .filter_map(|(pdu_id, pdu)| {
                    db.rooms
                        .pdu_count(&pdu_id)
                        .map(|pdu_count| (pdu_count, pdu))
                        .ok()
                })
                .take_while(|&(k, _)| Some(Ok(k)) != to) // Stop at `to`
                .collect::<Vec<_>>();

            let end_token = events_after.last().map(|(count, _)| count.to_string());

            let events_after = events_after
                .into_iter()
                .map(|(_, pdu)| pdu.to_room_event())
                .collect::<Vec<_>>();

            let mut resp = get_message_events::Response::new();
            resp.start = Some(body.from.to_owned());
            resp.end = end_token;
            resp.chunk = events_after;
            resp.state = Vec::new();

            Ok(resp.into())
        }
        get_message_events::Direction::Backward => {
            let events_before = db
                .rooms
                .pdus_until(sender_user, &body.room_id, from)?
                .take(limit)
                .filter_map(|r| r.ok()) // Filter out buggy events
                .filter_map(|(pdu_id, pdu)| {
                    db.rooms
                        .pdu_count(&pdu_id)
                        .map(|pdu_count| (pdu_count, pdu))
                        .ok()
                })
                .take_while(|&(k, _)| Some(Ok(k)) != to) // Stop at `to`
                .collect::<Vec<_>>();

            let start_token = events_before.last().map(|(count, _)| count.to_string());

            let events_before = events_before
                .into_iter()
                .map(|(_, pdu)| pdu.to_room_event())
                .collect::<Vec<_>>();

            let mut resp = get_message_events::Response::new();
            resp.start = Some(body.from.to_owned());
            resp.end = start_token;
            resp.chunk = events_before;
            resp.state = Vec::new();

            Ok(resp.into())
        }
    }
}
