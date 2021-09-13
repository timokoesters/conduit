use crate::{database::DatabaseGuard, pdu::PduBuilder, utils, ConduitResult, Error, Ruma};
use ruma::{
    api::{
        client::{
            error::ErrorKind,
            r0::profile::{
                get_avatar_url, get_display_name, get_profile, set_avatar_url, set_display_name,
            },
        },
        federation::{self, query::get_profile_information::v1::ProfileField},
    },
    events::EventType,
    serde::Raw,
};
use std::{convert::TryInto, sync::Arc};

#[cfg(feature = "conduit_bin")]
use rocket::{get, put};

/// # `PUT /_matrix/client/r0/profile/{userId}/displayname`
///
/// Updates the displayname.
///
/// - Also makes sure other users receive the update using presence EDUs
#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/profile/<_>/displayname", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn set_displayname_route(
    db: DatabaseGuard,
    body: Ruma<set_display_name::Request<'_>>,
) -> ConduitResult<set_display_name::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.users
        .set_displayname(sender_user, body.displayname.clone())?;

    // Send a new membership event and presence update into all joined rooms
    let all_rooms_joined: Vec<_> = db
        .rooms
        .rooms_joined(sender_user)
        .filter_map(|r| r.ok())
        .map(|room_id| {
            Ok::<_, Error>((
                PduBuilder {
                    event_type: EventType::RoomMember,
                    content: serde_json::to_value(ruma::events::room::member::MemberEventContent {
                        displayname: body.displayname.clone(),
                        ..serde_json::from_value::<Raw<_>>(
                            db.rooms
                                .room_state_get(
                                    &room_id,
                                    &EventType::RoomMember,
                                    &sender_user.to_string(),
                                )?
                                .ok_or_else(|| {
                                    Error::bad_database(
                                        "Tried to send displayname update for user not in the \
                                     room.",
                                    )
                                })?
                                .content
                                .clone(),
                        )
                        .expect("from_value::<Raw<..>> can never fail")
                        .deserialize()
                        .map_err(|_| Error::bad_database("Database contains invalid PDU."))?
                    })
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some(sender_user.to_string()),
                    redacts: None,
                },
                room_id,
            ))
        })
        .filter_map(|r| r.ok())
        .collect();

    for (pdu_builder, room_id) in all_rooms_joined {
        let mutex_state = Arc::clone(
            db.globals
                .roomid_mutex_state
                .write()
                .unwrap()
                .entry(room_id.clone())
                .or_default(),
        );
        let state_lock = mutex_state.lock().await;

        let _ = db
            .rooms
            .build_and_append_pdu(pdu_builder, sender_user, &room_id, &db, &state_lock);

        // Presence update
        db.rooms.edus.update_presence(
            sender_user,
            &room_id,
            ruma::events::presence::PresenceEvent {
                content: ruma::events::presence::PresenceEventContent {
                    avatar_url: db.users.avatar_url(sender_user)?,
                    currently_active: None,
                    displayname: db.users.displayname(sender_user)?,
                    last_active_ago: Some(
                        utils::millis_since_unix_epoch()
                            .try_into()
                            .expect("time is valid"),
                    ),
                    presence: ruma::presence::PresenceState::Online,
                    status_msg: None,
                },
                sender: sender_user.clone(),
            },
            &db.globals,
        )?;
    }

    db.flush()?;

    Ok(set_display_name::Response {}.into())
}

/// # `GET /_matrix/client/r0/profile/{userId}/displayname`
///
/// Returns the displayname of the user.
///
/// - If user is on another server: Fetches displayname over federation
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/profile/<_>/displayname", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_displayname_route(
    db: DatabaseGuard,
    body: Ruma<get_display_name::Request<'_>>,
) -> ConduitResult<get_display_name::Response> {
    if body.user_id.server_name() != db.globals.server_name() {
        let response = db
            .sending
            .send_federation_request(
                &db.globals,
                body.user_id.server_name(),
                federation::query::get_profile_information::v1::Request {
                    user_id: &body.user_id,
                    field: Some(&ProfileField::DisplayName),
                },
            )
            .await?;

        return Ok(get_display_name::Response {
            displayname: response.displayname,
        }
        .into());
    }

    Ok(get_display_name::Response {
        displayname: db.users.displayname(&body.user_id)?,
    }
    .into())
}

/// # `PUT /_matrix/client/r0/profile/{userId}/avatar_url`
///
/// Updates the avatar_url and blurhash.
///
/// - Also makes sure other users receive the update using presence EDUs
#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/profile/<_>/avatar_url", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn set_avatar_url_route(
    db: DatabaseGuard,
    body: Ruma<set_avatar_url::Request<'_>>,
) -> ConduitResult<set_avatar_url::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.users
        .set_avatar_url(sender_user, body.avatar_url.clone())?;

    db.users.set_blurhash(sender_user, body.blurhash.clone())?;

    // Send a new membership event and presence update into all joined rooms
    let all_joined_rooms: Vec<_> = db
        .rooms
        .rooms_joined(sender_user)
        .filter_map(|r| r.ok())
        .map(|room_id| {
            Ok::<_, Error>((
                PduBuilder {
                    event_type: EventType::RoomMember,
                    content: serde_json::to_value(ruma::events::room::member::MemberEventContent {
                        avatar_url: body.avatar_url.clone(),
                        ..serde_json::from_value::<Raw<_>>(
                            db.rooms
                                .room_state_get(
                                    &room_id,
                                    &EventType::RoomMember,
                                    &sender_user.to_string(),
                                )?
                                .ok_or_else(|| {
                                    Error::bad_database(
                                        "Tried to send displayname update for user not in the \
                                     room.",
                                    )
                                })?
                                .content
                                .clone(),
                        )
                        .expect("from_value::<Raw<..>> can never fail")
                        .deserialize()
                        .map_err(|_| Error::bad_database("Database contains invalid PDU."))?
                    })
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some(sender_user.to_string()),
                    redacts: None,
                },
                room_id,
            ))
        })
        .filter_map(|r| r.ok())
        .collect();

    for (pdu_builder, room_id) in all_joined_rooms {
        let mutex_state = Arc::clone(
            db.globals
                .roomid_mutex_state
                .write()
                .unwrap()
                .entry(room_id.clone())
                .or_default(),
        );
        let state_lock = mutex_state.lock().await;

        let _ = db
            .rooms
            .build_and_append_pdu(pdu_builder, sender_user, &room_id, &db, &state_lock);

        // Presence update
        db.rooms.edus.update_presence(
            sender_user,
            &room_id,
            ruma::events::presence::PresenceEvent {
                content: ruma::events::presence::PresenceEventContent {
                    avatar_url: db.users.avatar_url(sender_user)?,
                    currently_active: None,
                    displayname: db.users.displayname(sender_user)?,
                    last_active_ago: Some(
                        utils::millis_since_unix_epoch()
                            .try_into()
                            .expect("time is valid"),
                    ),
                    presence: ruma::presence::PresenceState::Online,
                    status_msg: None,
                },
                sender: sender_user.clone(),
            },
            &db.globals,
        )?;
    }

    db.flush()?;

    Ok(set_avatar_url::Response {}.into())
}

/// # `GET /_matrix/client/r0/profile/{userId}/avatar_url`
///
/// Returns the avatar_url and blurhash of the user.
///
/// - If user is on another server: Fetches avatar_url and blurhash over federation
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/profile/<_>/avatar_url", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_avatar_url_route(
    db: DatabaseGuard,
    body: Ruma<get_avatar_url::Request<'_>>,
) -> ConduitResult<get_avatar_url::Response> {
    if body.user_id.server_name() != db.globals.server_name() {
        let response = db
            .sending
            .send_federation_request(
                &db.globals,
                body.user_id.server_name(),
                federation::query::get_profile_information::v1::Request {
                    user_id: &body.user_id,
                    field: Some(&ProfileField::AvatarUrl),
                },
            )
            .await?;

        return Ok(get_avatar_url::Response {
            avatar_url: response.avatar_url,
            blurhash: response.blurhash,
        }
        .into());
    }

    Ok(get_avatar_url::Response {
        avatar_url: db.users.avatar_url(&body.user_id)?,
        blurhash: db.users.blurhash(&body.user_id)?,
    }
    .into())
}

/// # `GET /_matrix/client/r0/profile/{userId}`
///
/// Returns the displayname, avatar_url and blurhash of the user.
///
/// - If user is on another server: Fetches profile over federation
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/profile/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_profile_route(
    db: DatabaseGuard,
    body: Ruma<get_profile::Request<'_>>,
) -> ConduitResult<get_profile::Response> {
    if body.user_id.server_name() != db.globals.server_name() {
        let response = db
            .sending
            .send_federation_request(
                &db.globals,
                body.user_id.server_name(),
                federation::query::get_profile_information::v1::Request {
                    user_id: &body.user_id,
                    field: None,
                },
            )
            .await?;

        return Ok(get_profile::Response {
            displayname: response.displayname,
            avatar_url: response.avatar_url,
            blurhash: response.blurhash,
        }
        .into());
    }

    if !db.users.exists(&body.user_id)? {
        // Return 404 if this user doesn't exist
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Profile was not found.",
        ));
    }

    Ok(get_profile::Response {
        avatar_url: db.users.avatar_url(&body.user_id)?,
        blurhash: db.users.blurhash(&body.user_id)?,
        displayname: db.users.displayname(&body.user_id)?,
    }
    .into())
}
