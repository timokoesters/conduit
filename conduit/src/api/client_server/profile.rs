use crate::{Error, Result, Ruma, service::pdu::PduBuilder, services, utils};
use ruma::{
    api::{
        client::profile::{
            delete_profile_field, get_profile, get_profile_field, set_profile_field,
        },
        error::ErrorKind,
        federation,
    },
    events::{StateEventType, TimelineEventType},
    profile::{AvatarUrl, DisplayName, ProfileFieldName, ProfileFieldValue},
};
use serde_json::{Value as JsonValue, value::to_raw_value};
use std::{collections::BTreeMap, sync::Arc};

/// # `PUT /_matrix/client/r0/profile/{userId}/{keyName}`
///
/// Updates the specified profile field
///
/// - Also makes sure other users receive the update using presence EDUs, if the profile field
///   allows it
pub async fn set_profile_field_route(
    body: Ruma<set_profile_field::v3::Request>,
) -> Result<set_profile_field::v3::Response> {
    if body.sender.user_id != body.user_id {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Can't set profile information of other users",
        ));
    }

    let set_profile_field::v3::Request {
        user_id,
        value: profile_field,
    } = body.body;

    services()
        .users
        .set_profile_field(&user_id, &profile_field)?;

    // https://spec.matrix.org/v1.19/client-server-api/#events-on-change-of-profile-information
    // Basically only just send updates out for `displayname` and `avatar_url` updates
    if matches!(
        profile_field.field_name().clone(),
        ProfileFieldName::DisplayName | ProfileFieldName::AvatarUrl
    ) {
        // Send a new membership event and presence update into all joined rooms
        let all_joined_rooms: Vec<_> = services()
            .rooms
            .state_cache
            .rooms_joined(&user_id)
            .filter_map(|r| r.ok())
            .map(|room_id| {
                let mut event_content: BTreeMap<String, JsonValue> = serde_json::from_str(
                    services()
                        .rooms
                        .state_accessor
                        .room_state_get(&room_id, &StateEventType::RoomMember, user_id.as_str())?
                        .ok_or_else(|| {
                            Error::bad_database(
                                "Tried to send displayname update for user not in the \
                                     room.",
                            )
                        })?
                        .content
                        .get(),
                )
                .map_err(|_| Error::bad_database("Database contains invalid PDU."))?;
                event_content.insert(
                    profile_field.field_name().to_string(),
                    profile_field.value().into_owned(),
                );
                event_content.remove("join_authorized_via_users_server");

                Ok::<_, Error>((
                    PduBuilder {
                        event_type: TimelineEventType::RoomMember,
                        content: to_raw_value(&event_content)
                            .expect("event is valid, we just created it"),
                        unsigned: None,
                        state_key: Some(user_id.to_string()),
                        redacts: None,
                        timestamp: None,
                    },
                    room_id,
                ))
            })
            .filter_map(|r| r.ok())
            .collect();

        for (pdu_builder, room_id) in all_joined_rooms {
            let mutex_state = Arc::clone(
                services()
                    .globals
                    .roomid_mutex_state
                    .write()
                    .await
                    .entry(room_id.clone())
                    .or_default(),
            );
            let state_lock = mutex_state.lock().await;

            let _ = services()
                .rooms
                .timeline
                .build_and_append_pdu(pdu_builder, &user_id, &room_id, &state_lock)
                .await;

            if let Some((displayname, avatar_url)) = match profile_field.clone() {
                ProfileFieldValue::AvatarUrl(avatar_url) => Some((
                    services()
                        .users
                        .static_profile_field::<DisplayName>(&user_id)?,
                    Some(avatar_url),
                )),
                ProfileFieldValue::DisplayName(displayname) => Some((
                    Some(displayname),
                    services()
                        .users
                        .static_profile_field::<AvatarUrl>(&user_id)?,
                )),
                _ => None,
            } {
                // Presence update
                services().rooms.edus.presence.update_presence(
                    &user_id,
                    &room_id,
                    ruma::events::presence::PresenceEvent {
                        content: ruma::events::presence::PresenceEventContent {
                            avatar_url,
                            currently_active: None,
                            displayname,
                            last_active_ago: Some(
                                utils::millis_since_unix_epoch()
                                    .try_into()
                                    .expect("time is valid"),
                            ),
                            presence: ruma::presence::PresenceState::Online,
                            status_msg: None,
                        },
                        sender: user_id.clone(),
                    },
                )?;
            }
        }
    }

    Ok(set_profile_field::v3::Response {})
}

/// # `DELETE /_matrix/client/r0/profile/{userId}/{keyName}`
///
/// Removes the specified profile field
///
/// - Also makes sure other users receive the update using presence EDUs, if the profile field
///   allows it
pub async fn delete_profile_field_route(
    body: Ruma<delete_profile_field::v3::Request>,
) -> Result<delete_profile_field::v3::Response> {
    if body.sender.user_id != body.user_id {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Can't delete profile information of other users",
        ));
    }

    let delete_profile_field::v3::Request {
        user_id,
        field: profile_field,
    } = body.body;

    services()
        .users
        .delete_profile_field(&user_id, &profile_field)?;

    // https://spec.matrix.org/v1.19/client-server-api/#events-on-change-of-profile-information
    // Basically only just send updates out for `displayname` and `avatar_url` updates
    if matches!(
        profile_field.clone(),
        ProfileFieldName::DisplayName | ProfileFieldName::AvatarUrl
    ) {
        // Send a new membership event and presence update into all joined rooms
        let all_joined_rooms: Vec<_> = services()
            .rooms
            .state_cache
            .rooms_joined(&user_id)
            .filter_map(|r| r.ok())
            .map(|room_id| {
                let mut event_content: BTreeMap<String, JsonValue> = serde_json::from_str(
                    services()
                        .rooms
                        .state_accessor
                        .room_state_get(&room_id, &StateEventType::RoomMember, user_id.as_str())?
                        .ok_or_else(|| {
                            Error::bad_database(
                                "Tried to send displayname update for user not in the \
                                     room.",
                            )
                        })?
                        .content
                        .get(),
                )
                .map_err(|_| Error::bad_database("Database contains invalid PDU."))?;
                event_content.remove(profile_field.as_str());
                event_content.remove("join_authorized_via_users_server");

                Ok::<_, Error>((
                    PduBuilder {
                        event_type: TimelineEventType::RoomMember,
                        content: to_raw_value(&event_content)
                            .expect("event is valid, we just created it"),
                        unsigned: None,
                        state_key: Some(user_id.to_string()),
                        redacts: None,
                        timestamp: None,
                    },
                    room_id,
                ))
            })
            .filter_map(|r| r.ok())
            .collect();

        for (pdu_builder, room_id) in all_joined_rooms {
            let mutex_state = Arc::clone(
                services()
                    .globals
                    .roomid_mutex_state
                    .write()
                    .await
                    .entry(room_id.clone())
                    .or_default(),
            );
            let state_lock = mutex_state.lock().await;

            let _ = services()
                .rooms
                .timeline
                .build_and_append_pdu(pdu_builder, &user_id, &room_id, &state_lock)
                .await;

            if let Some((displayname, avatar_url)) = match profile_field {
                ProfileFieldName::AvatarUrl => Some((
                    services()
                        .users
                        .static_profile_field::<DisplayName>(&user_id)?,
                    None,
                )),
                ProfileFieldName::DisplayName => Some((
                    None,
                    services()
                        .users
                        .static_profile_field::<AvatarUrl>(&user_id)?,
                )),
                _ => None,
            } {
                // Presence update
                services().rooms.edus.presence.update_presence(
                    &user_id,
                    &room_id,
                    ruma::events::presence::PresenceEvent {
                        content: ruma::events::presence::PresenceEventContent {
                            avatar_url,
                            currently_active: None,
                            displayname,
                            last_active_ago: Some(
                                utils::millis_since_unix_epoch()
                                    .try_into()
                                    .expect("time is valid"),
                            ),
                            presence: ruma::presence::PresenceState::Online,
                            status_msg: None,
                        },
                        sender: user_id.clone(),
                    },
                )?;
            }
        }
    }

    Ok(delete_profile_field::v3::Response {})
}

/// # `GET /_matrix/client/v3/profile/{userId}`
///
/// Returns the displayname, avatar_url and blurhash of the user.
///
/// - If user is on another server: Fetches profile over federation
pub async fn get_profile_route(
    body: Ruma<get_profile::v3::Request>,
) -> Result<get_profile::v3::Response> {
    if body.user_id.server_name() != services().globals.server_name() {
        let response = services()
            .sending
            .send_federation_request(
                body.user_id.server_name(),
                federation::query::get_profile_information::v1::Request {
                    user_id: body.user_id.clone(),
                    field: None,
                },
            )
            .await?;

        return Ok(get_profile::v3::Response {
            data: response.data,
        });
    }

    if !services().users.exists(&body.user_id)? {
        // Return 404 if this user doesn't exist
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Profile was not found.",
        ));
    }

    Ok(get_profile::v3::Response {
        data: services().users.profile(&body.user_id)?,
    })
}

/// # `GET /_matrix/client/v3/profile/{userId}/{keyName}`
///
/// Get a field in the profile of the user.
pub async fn get_profile_field_route(
    body: Ruma<get_profile_field::v3::Request>,
) -> Result<get_profile_field::v3::Response> {
    if body.user_id.server_name() != services().globals.server_name() {
        let response = services()
            .sending
            .send_federation_request(
                body.user_id.server_name(),
                federation::query::get_profile_information::v1::Request {
                    user_id: body.user_id.clone(),
                    field: Some(body.field.clone()),
                },
            )
            .await?;

        let Some(value) = response.get(body.field.as_str()) else {
            return Ok(get_profile_field::v3::Response { value: None });
        };

        let Ok(value) = ProfileFieldValue::new(body.field.as_str(), value.clone()) else {
            return Err(Error::BadServerResponse(
                "Server returned field with invalid format",
            ));
        };

        return Ok(get_profile_field::v3::Response { value: Some(value) });
    }

    Ok(get_profile_field::v3::Response {
        value: services()
            .users
            .profile_field(&body.user_id, &body.field)?
            .map(|v| ProfileFieldValue::new(body.field.as_str(), v))
            .transpose()
            .map_err(|_| Error::bad_database("Had invalid value for known field"))?,
    })
}
