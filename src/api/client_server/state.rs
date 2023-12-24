use std::sync::Arc;

use crate::{service::pdu::PduBuilder, services, Error, Result, Ruma, RumaResponse};
use ruma::{
    api::client::{
        error::ErrorKind,
        state::{get_state_events, get_state_events_for_key, send_state_event},
    },
    events::{
        room::canonical_alias::RoomCanonicalAliasEventContent, AnyStateEventContent, StateEventType,
    },
    serde::Raw,
    EventId, RoomId, UserId,
};
use tracing::log::warn;

/// # `PUT /_matrix/client/r0/rooms/{roomId}/state/{eventType}/{stateKey}`
///
/// Sends a state event into the room.
///
/// - The only requirement for the content is that it has to be valid json
/// - Tries to send the event into the room, auth rules will determine if it is allowed
/// - If event is new canonical_alias: Rejects if alias is incorrect
pub async fn send_state_event_for_key_route(
    body: Ruma<send_state_event::v3::Request>,
) -> Result<send_state_event::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event_id = send_state_event_for_key_helper(
        sender_user,
        &body.room_id,
        &body.event_type,
        &body.body.body, // Yes, I hate it too
        body.state_key.to_owned(),
    )
    .await?;

    let event_id = (*event_id).to_owned();
    Ok(send_state_event::v3::Response { event_id })
}

/// # `PUT /_matrix/client/r0/rooms/{roomId}/state/{eventType}`
///
/// Sends a state event into the room.
///
/// - The only requirement for the content is that it has to be valid json
/// - Tries to send the event into the room, auth rules will determine if it is allowed
/// - If event is new canonical_alias: Rejects if alias is incorrect
pub async fn send_state_event_for_empty_key_route(
    body: Ruma<send_state_event::v3::Request>,
) -> Result<RumaResponse<send_state_event::v3::Response>> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    // Forbid m.room.encryption if encryption is disabled
    if body.event_type == StateEventType::RoomEncryption && !services().globals.allow_encryption() {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Encryption has been disabled",
        ));
    }

    let event_id = send_state_event_for_key_helper(
        sender_user,
        &body.room_id,
        &body.event_type.to_string().into(),
        &body.body.body,
        body.state_key.to_owned(),
    )
    .await?;

    let event_id = (*event_id).to_owned();
    Ok(send_state_event::v3::Response { event_id }.into())
}

/// # `GET /_matrix/client/r0/rooms/{roomid}/state`
///
/// Get all state events for a room.
///
/// - If not joined: Only works if current room history visibility is world readable
pub async fn get_state_events_route(
    body: Ruma<get_state_events::v3::Request>,
) -> Result<get_state_events::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !services()
        .rooms
        .state_accessor
        .user_can_see_state_events(sender_user, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view the room state.",
        ));
    }

    Ok(get_state_events::v3::Response {
        room_state: services()
            .rooms
            .state_accessor
            .room_state_full(&body.room_id)
            .await?
            .values()
            .map(|pdu| pdu.to_state_event())
            .collect(),
    })
}

/// # `GET /_matrix/client/r0/rooms/{roomid}/state/{eventType}/{stateKey}`
///
/// Get single state event of a room.
///
/// - If not joined: Only works if current room history visibility is world readable
pub async fn get_state_events_for_key_route(
    body: Ruma<get_state_events_for_key::v3::Request>,
) -> Result<get_state_events_for_key::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !services()
        .rooms
        .state_accessor
        .user_can_see_state_events(sender_user, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view the room state.",
        ));
    }

    let event = services()
        .rooms
        .state_accessor
        .room_state_get(&body.room_id, &body.event_type, &body.state_key)?
        .ok_or_else(|| {
            warn!(
                "State event {:?} not found in room {:?}",
                &body.event_type, &body.room_id
            );
            Error::BadRequest(ErrorKind::NotFound, "State event not found.")
        })?;

    Ok(get_state_events_for_key::v3::Response {
        content: serde_json::from_str(event.content.get())
            .map_err(|_| Error::bad_database("Invalid event content in database"))?,
    })
}

/// # `GET /_matrix/client/r0/rooms/{roomid}/state/{eventType}`
///
/// Get single state event of a room.
///
/// - If not joined: Only works if current room history visibility is world readable
pub async fn get_state_events_for_empty_key_route(
    body: Ruma<get_state_events_for_key::v3::Request>,
) -> Result<RumaResponse<get_state_events_for_key::v3::Response>> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !services()
        .rooms
        .state_accessor
        .user_can_see_state_events(sender_user, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view the room state.",
        ));
    }

    let event = services()
        .rooms
        .state_accessor
        .room_state_get(&body.room_id, &body.event_type, "")?
        .ok_or_else(|| {
            warn!(
                "State event {:?} not found in room {:?}",
                &body.event_type, &body.room_id
            );
            Error::BadRequest(ErrorKind::NotFound, "State event not found.")
        })?;

    Ok(get_state_events_for_key::v3::Response {
        content: serde_json::from_str(event.content.get())
            .map_err(|_| Error::bad_database("Invalid event content in database"))?,
    }
    .into())
}

async fn send_state_event_for_key_helper(
    sender: &UserId,
    room_id: &RoomId,
    event_type: &StateEventType,
    json: &Raw<AnyStateEventContent>,
    state_key: String,
) -> Result<Arc<EventId>> {
    let sender_user = sender;

    // TODO: Review this check, error if event is unparsable, use event type, allow alias if it
    // previously existed
    if let Ok(canonical_alias) =
        serde_json::from_str::<RoomCanonicalAliasEventContent>(json.json().get())
    {
        let mut aliases = canonical_alias.alt_aliases.clone();

        if let Some(alias) = canonical_alias.alias {
            aliases.push(alias);
        }

        for alias in aliases {
            if alias.server_name() != services().globals.server_name()
                || services()
                    .rooms
                    .alias
                    .resolve_local_alias(&alias)?
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
        services()
            .globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(room_id.to_owned())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    let event_id = services().rooms.timeline.build_and_append_pdu(
        PduBuilder {
            event_type: event_type.to_string().into(),
            content: serde_json::from_str(json.json().get()).expect("content is valid json"),
            unsigned: None,
            state_key: Some(state_key),
            redacts: None,
        },
        sender_user,
        room_id,
        &state_lock,
    )?;

    Ok(event_id)
}
