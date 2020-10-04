use super::State;
use crate::{pdu::PduBuilder, ConduitResult, Database, Error, Result, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::state::{
            get_state_events, get_state_events_for_empty_key, get_state_events_for_key,
            send_state_event_for_empty_key, send_state_event_for_key,
        },
    },
    events::{
        room::history_visibility::HistoryVisibility,
        room::history_visibility::HistoryVisibilityEventContent, AnyStateEventContent,
        EventContent, EventType,
    },
    EventId, RoomId, UserId,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, put};

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/rooms/<_>/state/<_>/<_>", data = "<body>")
)]
pub async fn send_state_event_for_key_route(
    db: State<'_, Database<'_>>,
    body: Ruma<send_state_event_for_key::Request<'_>>,
) -> ConduitResult<send_state_event_for_key::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let content = serde_json::from_str::<serde_json::Value>(
        body.json_body
            .as_ref()
            .ok_or(Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?
            .get(),
    )
    .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?;

    let event_id = send_state_event_for_key_helper(
        &db,
        sender_user,
        &body.content,
        content,
        &body.room_id,
        Some(body.state_key.to_owned()),
    )
    .await?;

    db.flush().await?;

    Ok(send_state_event_for_key::Response { event_id }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/rooms/<_>/state/<_>", data = "<body>")
)]
pub async fn send_state_event_for_empty_key_route(
    db: State<'_, Database<'_>>,
    body: Ruma<send_state_event_for_empty_key::Request<'_>>,
) -> ConduitResult<send_state_event_for_empty_key::Response> {
    // This just calls send_state_event_for_key_route
    let Ruma {
        body,
        sender_user,
        sender_device: _,
        json_body,
    } = body;

    let json = serde_json::from_str::<serde_json::Value>(
        json_body
            .as_ref()
            .ok_or(Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?
            .get(),
    )
    .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?;

    let event_id = send_state_event_for_key_helper(
        &db,
        sender_user
            .as_ref()
            .expect("no user for send state empty key rout"),
        &body.content,
        json,
        &body.room_id,
        Some("".into()),
    )
    .await?;

    db.flush().await?;

    Ok(send_state_event_for_empty_key::Response { event_id }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/state", data = "<body>")
)]
pub async fn get_state_events_route(
    db: State<'_, Database<'_>>,
    body: Ruma<get_state_events::Request>,
) -> ConduitResult<get_state_events::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    // Users not in the room should not be able to access the state unless history_visibility is
    // WorldReadable
    if !db.rooms.is_joined(sender_user, &body.room_id)? {
        if !matches!(
            db.rooms
                .room_state_get(&body.room_id, &EventType::RoomHistoryVisibility, "")?
                .map(|(_, event)| {
                    serde_json::from_value::<HistoryVisibilityEventContent>(event.content)
                        .map_err(|_| {
                            Error::bad_database(
                                "Invalid room history visibility event in database.",
                            )
                        })
                        .map(|e| e.history_visibility)
                }),
            Some(Ok(HistoryVisibility::WorldReadable))
        ) {
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "You don't have permission to view the room state.",
            ));
        }
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

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/state/<_>/<_>", data = "<body>")
)]
pub async fn get_state_events_for_key_route(
    db: State<'_, Database<'_>>,
    body: Ruma<get_state_events_for_key::Request>,
) -> ConduitResult<get_state_events_for_key::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    // Users not in the room should not be able to access the state unless history_visibility is
    // WorldReadable
    if !db.rooms.is_joined(sender_user, &body.room_id)? {
        if !matches!(
            db.rooms
                .room_state_get(&body.room_id, &EventType::RoomHistoryVisibility, "")?
                .map(|(_, event)| {
                    serde_json::from_value::<HistoryVisibilityEventContent>(event.content)
                        .map_err(|_| {
                            Error::bad_database(
                                "Invalid room history visibility event in database.",
                            )
                        })
                        .map(|e| e.history_visibility)
                }),
            Some(Ok(HistoryVisibility::WorldReadable))
        ) {
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "You don't have permission to view the room state.",
            ));
        }
    }

    let event = db
        .rooms
        .room_state_get(&body.room_id, &body.event_type, &body.state_key)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "State event not found.",
        ))?
        .1;

    Ok(get_state_events_for_key::Response {
        content: serde_json::value::to_raw_value(&event.content)
            .map_err(|_| Error::bad_database("Invalid event content in database"))?,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/state/<_>", data = "<body>")
)]
pub async fn get_state_events_for_empty_key_route(
    db: State<'_, Database<'_>>,
    body: Ruma<get_state_events_for_empty_key::Request>,
) -> ConduitResult<get_state_events_for_empty_key::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    // Users not in the room should not be able to access the state unless history_visibility is
    // WorldReadable
    if !db.rooms.is_joined(sender_user, &body.room_id)? {
        if !matches!(
            db.rooms
                .room_state_get(&body.room_id, &EventType::RoomHistoryVisibility, "")?
                .map(|(_, event)| {
                    serde_json::from_value::<HistoryVisibilityEventContent>(event.content)
                        .map_err(|_| {
                            Error::bad_database(
                                "Invalid room history visibility event in database.",
                            )
                        })
                        .map(|e| e.history_visibility)
                }),
            Some(Ok(HistoryVisibility::WorldReadable))
        ) {
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "You don't have permission to view the room state.",
            ));
        }
    }

    let event = db
        .rooms
        .room_state_get(&body.room_id, &body.event_type, "")?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "State event not found.",
        ))?
        .1;

    Ok(get_state_events_for_empty_key::Response {
        content: serde_json::value::to_raw_value(&event)
            .map_err(|_| Error::bad_database("Invalid event content in database"))?,
    }
    .into())
}

pub async fn send_state_event_for_key_helper(
    db: &Database<'_>,
    sender: &UserId,
    content: &AnyStateEventContent,
    json: serde_json::Value,
    room_id: &RoomId,
    state_key: Option<String>,
) -> Result<EventId> {
    let sender_user = sender;

    if let AnyStateEventContent::RoomCanonicalAlias(canonical_alias) = content {
        let mut aliases = canonical_alias.alt_aliases.clone();

        if let Some(alias) = canonical_alias.alias.clone() {
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

    let event_id = db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: content.event_type().into(),
            content: json,
            unsigned: None,
            state_key,
            redacts: None,
        },
        &sender_user,
        &room_id,
        &db.globals,
        &db.sending,
        &db.admin,
        &db.account_data,
    )?;

    Ok(event_id)
}
