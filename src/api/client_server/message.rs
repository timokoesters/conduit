use crate::{utils, Error, Result, Ruma, services, service::pdu::PduBuilder};
use ruma::{
    api::client::{
        error::ErrorKind,
        message::{get_message_events, send_message_event},
    },
    events::{RoomEventType, StateEventType},
};
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};

/// # `PUT /_matrix/client/r0/rooms/{roomId}/send/{eventType}/{txnId}`
///
/// Send a message event into the room.
///
/// - Is a NOOP if the txn id was already used before and returns the same event id again
/// - The only requirement for the content is that it has to be valid json
/// - Tries to send the event into the room, auth rules will determine if it is allowed
pub async fn send_message_event_route(
    body: Ruma<send_message_event::v3::IncomingRequest>,
) -> Result<send_message_event::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_deref();

    let mutex_state = Arc::clone(
        services().globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    // Forbid m.room.encrypted if encryption is disabled
    if RoomEventType::RoomEncrypted == body.event_type.to_string().into()
        && !services().globals.allow_encryption()
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Encryption has been disabled",
        ));
    }

    // Check if this is a new transaction id
    if let Some(response) =
        services().transaction_ids
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

        let event_id = utils::string_from_bytes(&response)
            .map_err(|_| Error::bad_database("Invalid txnid bytes in database."))?
            .try_into()
            .map_err(|_| Error::bad_database("Invalid event id in txnid data."))?;
        return Ok(send_message_event::v3::Response { event_id });
    }

    let mut unsigned = BTreeMap::new();
    unsigned.insert("transaction_id".to_owned(), body.txn_id.to_string().into());

    let event_id = services().rooms.build_and_append_pdu(
        PduBuilder {
            event_type: body.event_type.to_string().into(),
            content: serde_json::from_str(body.body.body.json().get())
                .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?,
            unsigned: Some(unsigned),
            state_key: None,
            redacts: None,
        },
        sender_user,
        &body.room_id,
        &state_lock,
    )?;

    services().transaction_ids.add_txnid(
        sender_user,
        sender_device,
        &body.txn_id,
        event_id.as_bytes(),
    )?;

    drop(state_lock);

    Ok(send_message_event::v3::Response::new(
        (*event_id).to_owned(),
    ))
}

/// # `GET /_matrix/client/r0/rooms/{roomId}/messages`
///
/// Allows paginating through room history.
///
/// - Only works if the user is joined (TODO: always allow, but only show events where the user was
/// joined, depending on history_visibility)
pub async fn get_message_events_route(
    body: Ruma<get_message_events::v3::IncomingRequest>,
) -> Result<get_message_events::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    if !services().rooms.is_joined(sender_user, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    let from = match body.from.clone() {
        Some(from) => from
            .parse()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid `from` value."))?,

        None => match body.dir {
            get_message_events::v3::Direction::Forward => 0,
            get_message_events::v3::Direction::Backward => u64::MAX,
        },
    };

    let to = body.to.as_ref().map(|t| t.parse());

    services().rooms
        .lazy_load_confirm_delivery(sender_user, sender_device, &body.room_id, from)?;

    // Use limit or else 10
    let limit = body.limit.try_into().map_or(10_usize, |l: u32| l as usize);

    let next_token;

    let mut resp = get_message_events::v3::Response::new();

    let mut lazy_loaded = HashSet::new();

    match body.dir {
        get_message_events::v3::Direction::Forward => {
            let events_after: Vec<_> = services()
                .rooms
                .pdus_after(sender_user, &body.room_id, from)?
                .take(limit)
                .filter_map(|r| r.ok()) // Filter out buggy events
                .filter_map(|(pdu_id, pdu)| {
                    services().rooms
                        .pdu_count(&pdu_id)
                        .map(|pdu_count| (pdu_count, pdu))
                        .ok()
                })
                .take_while(|&(k, _)| Some(Ok(k)) != to) // Stop at `to`
                .collect();

            for (_, event) in &events_after {
                if !services().rooms.lazy_load_was_sent_before(
                    sender_user,
                    sender_device,
                    &body.room_id,
                    &event.sender,
                )? {
                    lazy_loaded.insert(event.sender.clone());
                }
            }

            next_token = events_after.last().map(|(count, _)| count).copied();

            let events_after: Vec<_> = events_after
                .into_iter()
                .map(|(_, pdu)| pdu.to_room_event())
                .collect();

            resp.start = from.to_string();
            resp.end = next_token.map(|count| count.to_string());
            resp.chunk = events_after;
        }
        get_message_events::v3::Direction::Backward => {
            let events_before: Vec<_> = services()
                .rooms
                .pdus_until(sender_user, &body.room_id, from)?
                .take(limit)
                .filter_map(|r| r.ok()) // Filter out buggy events
                .filter_map(|(pdu_id, pdu)| {
                    services().rooms
                        .pdu_count(&pdu_id)
                        .map(|pdu_count| (pdu_count, pdu))
                        .ok()
                })
                .take_while(|&(k, _)| Some(Ok(k)) != to) // Stop at `to`
                .collect();

            for (_, event) in &events_before {
                if !services().rooms.lazy_load_was_sent_before(
                    sender_user,
                    sender_device,
                    &body.room_id,
                    &event.sender,
                )? {
                    lazy_loaded.insert(event.sender.clone());
                }
            }

            next_token = events_before.last().map(|(count, _)| count).copied();

            let events_before: Vec<_> = events_before
                .into_iter()
                .map(|(_, pdu)| pdu.to_room_event())
                .collect();

            resp.start = from.to_string();
            resp.end = next_token.map(|count| count.to_string());
            resp.chunk = events_before;
        }
    }

    resp.state = Vec::new();
    for ll_id in &lazy_loaded {
        if let Some(member_event) =
            services().rooms
                .room_state_get(&body.room_id, &StateEventType::RoomMember, ll_id.as_str())?
        {
            resp.state.push(member_event.to_state_event());
        }
    }

    if let Some(next_token) = next_token {
        services().rooms.lazy_load_mark_sent(
            sender_user,
            sender_device,
            &body.room_id,
            lazy_loaded,
            next_token,
        );
    }

    Ok(resp)
}
