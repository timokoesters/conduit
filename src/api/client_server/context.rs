use crate::{database::DatabaseGuard, Error, Result, Ruma};
use ruma::{
    api::client::{context::get_context, error::ErrorKind, filter::LazyLoadOptions},
    events::StateEventType,
};
use std::{collections::HashSet, convert::TryFrom};
use tracing::error;

/// # `GET /_matrix/client/r0/rooms/{roomId}/context`
///
/// Allows loading room history around an event.
///
/// - Only works if the user is joined (TODO: always allow, but only show events if the user was
/// joined, depending on history_visibility)
pub async fn get_context_route(
    db: DatabaseGuard,
    body: Ruma<get_context::v3::IncomingRequest>,
) -> Result<get_context::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    let (lazy_load_enabled, lazy_load_send_redundant) = match &body.filter.lazy_load_options {
        LazyLoadOptions::Enabled {
            include_redundant_members,
        } => (true, *include_redundant_members),
        _ => (false, false),
    };

    let mut lazy_loaded = HashSet::new();

    let base_pdu_id = db
        .rooms
        .get_pdu_id(&body.event_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Base event id not found.",
        ))?;

    let base_token = db.rooms.pdu_count(&base_pdu_id)?;

    let base_event = db
        .rooms
        .get_pdu_from_id(&base_pdu_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Base event not found.",
        ))?;

    let room_id = base_event.room_id.clone();

    if !db.rooms.is_joined(sender_user, &room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    if !db.rooms.lazy_load_was_sent_before(
        sender_user,
        sender_device,
        &room_id,
        &base_event.sender,
    )? || lazy_load_send_redundant
    {
        lazy_loaded.insert(base_event.sender.as_str().to_owned());
    }

    let base_event = base_event.to_room_event();

    let events_before: Vec<_> = db
        .rooms
        .pdus_until(sender_user, &room_id, base_token)?
        .take(
            u32::try_from(body.limit).map_err(|_| {
                Error::BadRequest(ErrorKind::InvalidParam, "Limit value is invalid.")
            })? as usize
                / 2,
        )
        .filter_map(|r| r.ok()) // Remove buggy events
        .collect();

    for (_, event) in &events_before {
        if !db.rooms.lazy_load_was_sent_before(
            sender_user,
            sender_device,
            &room_id,
            &event.sender,
        )? || lazy_load_send_redundant
        {
            lazy_loaded.insert(event.sender.as_str().to_owned());
        }
    }

    let start_token = events_before
        .last()
        .and_then(|(pdu_id, _)| db.rooms.pdu_count(pdu_id).ok())
        .map(|count| count.to_string());

    let events_before: Vec<_> = events_before
        .into_iter()
        .map(|(_, pdu)| pdu.to_room_event())
        .collect();

    let events_after: Vec<_> = db
        .rooms
        .pdus_after(sender_user, &room_id, base_token)?
        .take(
            u32::try_from(body.limit).map_err(|_| {
                Error::BadRequest(ErrorKind::InvalidParam, "Limit value is invalid.")
            })? as usize
                / 2,
        )
        .filter_map(|r| r.ok()) // Remove buggy events
        .collect();

    for (_, event) in &events_after {
        if !db.rooms.lazy_load_was_sent_before(
            sender_user,
            sender_device,
            &room_id,
            &event.sender,
        )? || lazy_load_send_redundant
        {
            lazy_loaded.insert(event.sender.as_str().to_owned());
        }
    }

    let shortstatehash = match db.rooms.pdu_shortstatehash(
        events_after
            .last()
            .map_or(&*body.event_id, |(_, e)| &*e.event_id),
    )? {
        Some(s) => s,
        None => db
            .rooms
            .current_shortstatehash(&room_id)?
            .expect("All rooms have state"),
    };

    let state_ids = db.rooms.state_full_ids(shortstatehash).await?;

    let end_token = events_after
        .last()
        .and_then(|(pdu_id, _)| db.rooms.pdu_count(pdu_id).ok())
        .map(|count| count.to_string());

    let events_after: Vec<_> = events_after
        .into_iter()
        .map(|(_, pdu)| pdu.to_room_event())
        .collect();

    let mut state = Vec::new();

    for (shortstatekey, id) in state_ids {
        let (event_type, state_key) = db.rooms.get_statekey_from_short(shortstatekey)?;

        if event_type != StateEventType::RoomMember {
            let pdu = match db.rooms.get_pdu(&id)? {
                Some(pdu) => pdu,
                None => {
                    error!("Pdu in state not found: {}", id);
                    continue;
                }
            };
            state.push(pdu.to_state_event());
        } else if !lazy_load_enabled || lazy_loaded.contains(&state_key) {
            let pdu = match db.rooms.get_pdu(&id)? {
                Some(pdu) => pdu,
                None => {
                    error!("Pdu in state not found: {}", id);
                    continue;
                }
            };
            state.push(pdu.to_state_event());
        }
    }

    let resp = get_context::v3::Response {
        start: start_token,
        end: end_token,
        events_before,
        event: Some(base_event),
        events_after,
        state,
    };

    Ok(resp)
}
