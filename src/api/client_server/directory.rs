use crate::{Error, Result, Ruma, services};
use ruma::{
    api::{
        client::{
            directory::{
                get_public_rooms, get_public_rooms_filtered, get_room_visibility,
                set_room_visibility,
            },
            error::ErrorKind,
            room,
        },
        federation,
    },
    directory::{
        Filter, IncomingFilter, IncomingRoomNetwork, PublicRoomJoinRule, PublicRoomsChunk,
        RoomNetwork,
    },
    events::{
        room::{
            avatar::RoomAvatarEventContent,
            canonical_alias::RoomCanonicalAliasEventContent,
            guest_access::{GuestAccess, RoomGuestAccessEventContent},
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            name::RoomNameEventContent,
            topic::RoomTopicEventContent,
        },
        StateEventType,
    },
    ServerName, UInt,
};
use tracing::{info, warn};

/// # `POST /_matrix/client/r0/publicRooms`
///
/// Lists the public rooms on this server.
///
/// - Rooms are ordered by the number of joined members
pub async fn get_public_rooms_filtered_route(
    body: Ruma<get_public_rooms_filtered::v3::IncomingRequest>,
) -> Result<get_public_rooms_filtered::v3::Response> {
    get_public_rooms_filtered_helper(
        body.server.as_deref(),
        body.limit,
        body.since.as_deref(),
        &body.filter,
        &body.room_network,
    )
    .await
}

/// # `GET /_matrix/client/r0/publicRooms`
///
/// Lists the public rooms on this server.
///
/// - Rooms are ordered by the number of joined members
pub async fn get_public_rooms_route(
    body: Ruma<get_public_rooms::v3::IncomingRequest>,
) -> Result<get_public_rooms::v3::Response> {
    let response = get_public_rooms_filtered_helper(
        body.server.as_deref(),
        body.limit,
        body.since.as_deref(),
        &IncomingFilter::default(),
        &IncomingRoomNetwork::Matrix,
    )
    .await?;

    Ok(get_public_rooms::v3::Response {
        chunk: response.chunk,
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    })
}

/// # `PUT /_matrix/client/r0/directory/list/room/{roomId}`
///
/// Sets the visibility of a given room in the room directory.
///
/// - TODO: Access control checks
pub async fn set_room_visibility_route(
    body: Ruma<set_room_visibility::v3::IncomingRequest>,
) -> Result<set_room_visibility::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    match &body.visibility {
        room::Visibility::Public => {
            services().rooms.set_public(&body.room_id, true)?;
            info!("{} made {} public", sender_user, body.room_id);
        }
        room::Visibility::Private => services().rooms.set_public(&body.room_id, false)?,
        _ => {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Room visibility type is not supported.",
            ));
        }
    }

    Ok(set_room_visibility::v3::Response {})
}

/// # `GET /_matrix/client/r0/directory/list/room/{roomId}`
///
/// Gets the visibility of a given room in the room directory.
pub async fn get_room_visibility_route(
    body: Ruma<get_room_visibility::v3::IncomingRequest>,
) -> Result<get_room_visibility::v3::Response> {
    Ok(get_room_visibility::v3::Response {
        visibility: if services().rooms.is_public_room(&body.room_id)? {
            room::Visibility::Public
        } else {
            room::Visibility::Private
        },
    })
}

pub(crate) async fn get_public_rooms_filtered_helper(
    server: Option<&ServerName>,
    limit: Option<UInt>,
    since: Option<&str>,
    filter: &IncomingFilter,
    _network: &IncomingRoomNetwork,
) -> Result<get_public_rooms_filtered::v3::Response> {
    if let Some(other_server) = server.filter(|server| *server != services().globals.server_name().as_str())
    {
        let response = services()
            .sending
            .send_federation_request(
                other_server,
                federation::directory::get_public_rooms_filtered::v1::Request {
                    limit,
                    since,
                    filter: Filter {
                        generic_search_term: filter.generic_search_term.as_deref(),
                    },
                    room_network: RoomNetwork::Matrix,
                },
            )
            .await?;

        return Ok(get_public_rooms_filtered::v3::Response {
            chunk: response.chunk,
            prev_batch: response.prev_batch,
            next_batch: response.next_batch,
            total_room_count_estimate: response.total_room_count_estimate,
        });
    }

    let limit = limit.map_or(10, u64::from);
    let mut num_since = 0_u64;

    if let Some(s) = &since {
        let mut characters = s.chars();
        let backwards = match characters.next() {
            Some('n') => false,
            Some('p') => true,
            _ => {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Invalid `since` token",
                ))
            }
        };

        num_since = characters
            .collect::<String>()
            .parse()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid `since` token."))?;

        if backwards {
            num_since = num_since.saturating_sub(limit);
        }
    }

    let mut all_rooms: Vec<_> = services()
        .rooms
        .public_rooms()
        .map(|room_id| {
            let room_id = room_id?;

            let chunk = PublicRoomsChunk {
                canonical_alias: services()
                    .rooms
                    .room_state_get(&room_id, &StateEventType::RoomCanonicalAlias, "")?
                    .map_or(Ok(None), |s| {
                        serde_json::from_str(s.content.get())
                            .map(|c: RoomCanonicalAliasEventContent| c.alias)
                            .map_err(|_| {
                                Error::bad_database("Invalid canonical alias event in database.")
                            })
                    })?,
                name: services()
                    .rooms
                    .room_state_get(&room_id, &StateEventType::RoomName, "")?
                    .map_or(Ok(None), |s| {
                        serde_json::from_str(s.content.get())
                            .map(|c: RoomNameEventContent| c.name)
                            .map_err(|_| {
                                Error::bad_database("Invalid room name event in database.")
                            })
                    })?,
                num_joined_members: services()
                    .rooms
                    .room_joined_count(&room_id)?
                    .unwrap_or_else(|| {
                        warn!("Room {} has no member count", room_id);
                        0
                    })
                    .try_into()
                    .expect("user count should not be that big"),
                topic: services()
                    .rooms
                    .room_state_get(&room_id, &StateEventType::RoomTopic, "")?
                    .map_or(Ok(None), |s| {
                        serde_json::from_str(s.content.get())
                            .map(|c: RoomTopicEventContent| Some(c.topic))
                            .map_err(|_| {
                                Error::bad_database("Invalid room topic event in database.")
                            })
                    })?,
                world_readable: services()
                    .rooms
                    .room_state_get(&room_id, &StateEventType::RoomHistoryVisibility, "")?
                    .map_or(Ok(false), |s| {
                        serde_json::from_str(s.content.get())
                            .map(|c: RoomHistoryVisibilityEventContent| {
                                c.history_visibility == HistoryVisibility::WorldReadable
                            })
                            .map_err(|_| {
                                Error::bad_database(
                                    "Invalid room history visibility event in database.",
                                )
                            })
                    })?,
                guest_can_join: services()
                    .rooms
                    .room_state_get(&room_id, &StateEventType::RoomGuestAccess, "")?
                    .map_or(Ok(false), |s| {
                        serde_json::from_str(s.content.get())
                            .map(|c: RoomGuestAccessEventContent| {
                                c.guest_access == GuestAccess::CanJoin
                            })
                            .map_err(|_| {
                                Error::bad_database("Invalid room guest access event in database.")
                            })
                    })?,
                avatar_url: services()
                    .rooms
                    .room_state_get(&room_id, &StateEventType::RoomAvatar, "")?
                    .map(|s| {
                        serde_json::from_str(s.content.get())
                            .map(|c: RoomAvatarEventContent| c.url)
                            .map_err(|_| {
                                Error::bad_database("Invalid room avatar event in database.")
                            })
                    })
                    .transpose()?
                    // url is now an Option<String> so we must flatten
                    .flatten(),
                join_rule: services()
                    .rooms
                    .room_state_get(&room_id, &StateEventType::RoomJoinRules, "")?
                    .map(|s| {
                        serde_json::from_str(s.content.get())
                            .map(|c: RoomJoinRulesEventContent| match c.join_rule {
                                JoinRule::Public => Some(PublicRoomJoinRule::Public),
                                JoinRule::Knock => Some(PublicRoomJoinRule::Knock),
                                _ => None,
                            })
                            .map_err(|_| {
                                Error::bad_database("Invalid room join rule event in database.")
                            })
                    })
                    .transpose()?
                    .flatten()
                    .ok_or(Error::bad_database(
                        "Invalid room join rule event in database.",
                    ))?,
                room_id,
            };
            Ok(chunk)
        })
        .filter_map(|r: Result<_>| r.ok()) // Filter out buggy rooms
        .filter(|chunk| {
            if let Some(query) = filter
                .generic_search_term
                .as_ref()
                .map(|q| q.to_lowercase())
            {
                if let Some(name) = &chunk.name {
                    if name.as_str().to_lowercase().contains(&query) {
                        return true;
                    }
                }

                if let Some(topic) = &chunk.topic {
                    if topic.to_lowercase().contains(&query) {
                        return true;
                    }
                }

                if let Some(canonical_alias) = &chunk.canonical_alias {
                    if canonical_alias.as_str().to_lowercase().contains(&query) {
                        return true;
                    }
                }

                false
            } else {
                // No search term
                true
            }
        })
        // We need to collect all, so we can sort by member count
        .collect();

    all_rooms.sort_by(|l, r| r.num_joined_members.cmp(&l.num_joined_members));

    let total_room_count_estimate = (all_rooms.len() as u32).into();

    let chunk: Vec<_> = all_rooms
        .into_iter()
        .skip(num_since as usize)
        .take(limit as usize)
        .collect();

    let prev_batch = if num_since == 0 {
        None
    } else {
        Some(format!("p{}", num_since))
    };

    let next_batch = if chunk.len() < limit as usize {
        None
    } else {
        Some(format!("n{}", num_since + limit))
    };

    Ok(get_public_rooms_filtered::v3::Response {
        chunk,
        prev_batch,
        next_batch,
        total_room_count_estimate: Some(total_room_count_estimate),
    })
}
