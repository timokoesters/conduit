use std::sync::Arc;

use crate::{
    database::DatabaseGuard, pdu::PduBuilder, ConduitResult, Database, Error, Result, Ruma,
};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::state::{get_state_events, get_state_events_for_key, send_state_event},
    },
    events::{
        room::{
            canonical_alias::CanonicalAliasEventContent,
            history_visibility::{HistoryVisibility, HistoryVisibilityEventContent},
        },
        AnyStateEventContent, EventType,
    },
    serde::Raw,
    EventId, RoomId, UserId,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, put};

/// # `PUT /_matrix/client/r0/rooms/{roomId}/state/{eventType}/{stateKey}`
///
/// Sends a state event into the room.
///
/// - The only requirement for the content is that it has to be valid json
/// - Tries to send the event into the room, auth rules will determine if it is allowed
/// - If event is new canonical_alias: Rejects if alias is incorrect
#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/rooms/<_>/state/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn send_state_event_for_key_route(
    db: DatabaseGuard,
    body: Ruma<send_state_event::Request<'_>>,
) -> ConduitResult<send_state_event::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event_id = send_state_event_for_key_helper(
        &db,
        sender_user,
        &body.room_id,
        EventType::from(&body.event_type),
        &body.body.body, // Yes, I hate it too
        body.state_key.to_owned(),
    )
    .await?;

    db.flush()?;

    Ok(send_state_event::Response { event_id }.into())
}

/// # `PUT /_matrix/client/r0/rooms/{roomId}/state/{eventType}`
///
/// Sends a state event into the room.
///
/// - The only requirement for the content is that it has to be valid json
/// - Tries to send the event into the room, auth rules will determine if it is allowed
/// - If event is new canonical_alias: Rejects if alias is incorrect
#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/rooms/<_>/state/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn send_state_event_for_empty_key_route(
    db: DatabaseGuard,
    body: Ruma<send_state_event::Request<'_>>,
) -> ConduitResult<send_state_event::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event_id = send_state_event_for_key_helper(
        &db,
        sender_user,
        &body.room_id,
        EventType::from(&body.event_type),
        &body.body.body,
        body.state_key.to_owned(),
    )
    .await?;

    db.flush()?;

    Ok(send_state_event::Response { event_id }.into())
}

/// # `GET /_matrix/client/r0/rooms/{roomid}/state`
///
/// Get all state events for a room.
///
/// - If not joined: Only works if current room history visibility is world readable
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/state", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_state_events_route(
    db: DatabaseGuard,
    body: Ruma<get_state_events::Request<'_>>,
) -> ConduitResult<get_state_events::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    #[allow(clippy::blocks_in_if_conditions)]
    // Users not in the room should not be able to access the state unless history_visibility is
    // WorldReadable
    if !db.rooms.is_joined(sender_user, &body.room_id)?
        && !matches!(
            db.rooms
                .room_state_get(&body.room_id, &EventType::RoomHistoryVisibility, "")?
                .map(|event| {
                    serde_json::from_str::<HistoryVisibilityEventContent>(event.content.get())
                        .map_err(|_| {
                            Error::bad_database(
                                "Invalid room history visibility event in database.",
                            )
                        })
                        .map(|e| e.history_visibility)
                }),
            Some(Ok(HistoryVisibility::WorldReadable))
        )
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view the room state.",
        ));
    }

    Ok(get_state_events::Response {
        room_state: db
            .rooms
            .room_state_full(&body.room_id)?
            .values()
            .map(|pdu| pdu.to_state_event())
            .collect(),
    }
    .into())
}

/// # `GET /_matrix/client/r0/rooms/{roomid}/state/{eventType}/{stateKey}`
///
/// Get single state event of a room.
///
/// - If not joined: Only works if current room history visibility is world readable
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/state/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_state_events_for_key_route(
    db: DatabaseGuard,
    body: Ruma<get_state_events_for_key::Request<'_>>,
) -> ConduitResult<get_state_events_for_key::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    #[allow(clippy::blocks_in_if_conditions)]
    // Users not in the room should not be able to access the state unless history_visibility is
    // WorldReadable
    if !db.rooms.is_joined(sender_user, &body.room_id)?
        && !matches!(
            db.rooms
                .room_state_get(&body.room_id, &EventType::RoomHistoryVisibility, "")?
                .map(|event| {
                    serde_json::from_str::<HistoryVisibilityEventContent>(event.content.get())
                        .map_err(|_| {
                            Error::bad_database(
                                "Invalid room history visibility event in database.",
                            )
                        })
                        .map(|e| e.history_visibility)
                }),
            Some(Ok(HistoryVisibility::WorldReadable))
        )
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view the room state.",
        ));
    }

    let event = db
        .rooms
        .room_state_get(&body.room_id, &body.event_type, &body.state_key)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "State event not found.",
        ))?;

    Ok(get_state_events_for_key::Response {
        content: serde_json::from_str(event.content.get())
            .map_err(|_| Error::bad_database("Invalid event content in database"))?,
    }
    .into())
}

/// # `GET /_matrix/client/r0/rooms/{roomid}/state/{eventType}`
///
/// Get single state event of a room.
///
/// - If not joined: Only works if current room history visibility is world readable
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/state/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_state_events_for_empty_key_route(
    db: DatabaseGuard,
    body: Ruma<get_state_events_for_key::Request<'_>>,
) -> ConduitResult<get_state_events_for_key::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    #[allow(clippy::blocks_in_if_conditions)]
    // Users not in the room should not be able to access the state unless history_visibility is
    // WorldReadable
    if !db.rooms.is_joined(sender_user, &body.room_id)?
        && !matches!(
            db.rooms
                .room_state_get(&body.room_id, &EventType::RoomHistoryVisibility, "")?
                .map(|event| {
                    serde_json::from_str::<HistoryVisibilityEventContent>(event.content.get())
                        .map_err(|_| {
                            Error::bad_database(
                                "Invalid room history visibility event in database.",
                            )
                        })
                        .map(|e| e.history_visibility)
                }),
            Some(Ok(HistoryVisibility::WorldReadable))
        )
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view the room state.",
        ));
    }

    let event = db
        .rooms
        .room_state_get(&body.room_id, &body.event_type, "")?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "State event not found.",
        ))?;

    Ok(get_state_events_for_key::Response {
        content: serde_json::from_str(event.content.get())
            .map_err(|_| Error::bad_database("Invalid event content in database"))?,
    }
    .into())
}

async fn send_state_event_for_key_helper(
    db: &Database,
    sender: &UserId,
    room_id: &RoomId,
    event_type: EventType,
    json: &Raw<AnyStateEventContent>,
    state_key: String,
) -> Result<EventId> {
    let sender_user = sender;

    // TODO: Review this check, error if event is unparsable, use event type, allow alias if it
    // previously existed
    if let Ok(canonical_alias) =
        serde_json::from_str::<CanonicalAliasEventContent>(json.json().get())
    {
        let mut aliases = canonical_alias.alt_aliases.clone();

        if let Some(alias) = canonical_alias.alias {
            aliases.push(alias);
        }

        for alias in aliases {
            if alias.server_name() != db.globals.server_name()
                || db
                    .rooms
                    .id_from_alias(&alias)?
                    .filter(|room| room == room_id) // Make sure it's the right room
                    .is_none()
            {
                return Err(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "You are only allowed to send canonical_alias \
                    events when it's aliases already exists",
                ));
            }
        }
    }

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    let event_id = db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type,
            content: serde_json::from_str(json.json().get()).expect("content is valid json"),
            unsigned: None,
            state_key: Some(state_key),
            redacts: None,
        },
        sender_user,
        room_id,
        db,
        &state_lock,
    )?;

    Ok(event_id)
}
