use super::State;
use crate::{pdu::PduBuilder, ConduitResult, Database, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::message::{get_message_events, send_message_event},
    },
    events::EventContent,
};
use std::convert::TryInto;

#[cfg(feature = "conduit_bin")]
use rocket::{get, put};

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/rooms/<_>/send/<_>/<_>", data = "<body>")
)]
pub fn send_message_event_route(
    db: State<'_, Database>,
    body: Ruma<send_message_event::IncomingRequest>,
) -> ConduitResult<send_message_event::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    let mut unsigned = serde_json::Map::new();
    unsigned.insert("transaction_id".to_owned(), body.txn_id.clone().into());

    let event_id = db.rooms.build_and_append_pdu(
        PduBuilder {
            room_id: body.room_id.clone(),
            sender: sender_id.clone(),
            event_type: body.content.event_type().into(),
            content: serde_json::from_str(
                body.json_body
                    .ok_or(Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?
                    .get(),
            )
            .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?,
            unsigned: Some(unsigned),
            state_key: None,
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    Ok(send_message_event::Response::new(event_id).into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/messages", data = "<body>")
)]
pub fn get_message_events_route(
    db: State<'_, Database>,
    body: Ruma<get_message_events::IncomingRequest>,
) -> ConduitResult<get_message_events::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(sender_id, &body.room_id)? {
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
                .pdus_after(&sender_id, &body.room_id, from)
                .take(limit)
                .filter_map(|r| r.ok()) // Filter out buggy events
                .take_while(|&(k, _)| Some(Ok(k)) != to) // Stop at `to`
                .collect::<Vec<_>>();

            let end_token = events_after.last().map(|(count, _)| count.to_string());

            let events_after = events_after
                .into_iter()
                .map(|(_, pdu)| pdu.to_room_event())
                .collect::<Vec<_>>();

            let mut resp = get_message_events::Response::new();
            resp.start = Some(body.from.clone());
            resp.end = end_token;
            resp.chunk = events_after;
            resp.state = Vec::new();

            Ok(resp.into())
        }
        get_message_events::Direction::Backward => {
            let events_before = db
                .rooms
                .pdus_until(&sender_id, &body.room_id, from)
                .take(limit)
                .filter_map(|r| r.ok()) // Filter out buggy events
                .take_while(|&(k, _)| Some(Ok(k)) != to) // Stop at `to`
                .collect::<Vec<_>>();

            let start_token = events_before.last().map(|(count, _)| count.to_string());

            let events_before = events_before
                .into_iter()
                .map(|(_, pdu)| pdu.to_room_event())
                .collect::<Vec<_>>();

            let mut resp = get_message_events::Response::new();
            resp.start = Some(body.from.clone());
            resp.end = start_token;
            resp.chunk = events_before;
            resp.state = Vec::new();

            Ok(resp.into())
        }
    }
}
