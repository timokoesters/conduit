use std::convert::TryInto;

use crate::{database::DatabaseGuard, ConduitResult, Database, Error, Result, Ruma};
use ruma::{
    api::{
        client::{
            error::ErrorKind,
            r0::{
                directory::{
                    get_public_rooms, get_public_rooms_filtered, get_room_visibility,
                    set_room_visibility,
                },
                room,
            },
        },
        federation,
    },
    directory::{Filter, IncomingFilter, IncomingRoomNetwork, PublicRoomsChunk, RoomNetwork},
    events::{
        room::{avatar, canonical_alias, guest_access, history_visibility, name, topic},
        EventType,
    },
    serde::Raw,
    ServerName, UInt,
};
use tracing::{info, warn};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post, put};

/// # `POST /_matrix/client/r0/publicRooms`
///
/// Lists the public rooms on this server.
///
/// - Rooms are ordered by the number of joined members
#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/publicRooms", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_public_rooms_filtered_route(
    db: DatabaseGuard,
    body: Ruma<get_public_rooms_filtered::Request<'_>>,
) -> ConduitResult<get_public_rooms_filtered::Response> {
    get_public_rooms_filtered_helper(
        &db,
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
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/publicRooms", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_public_rooms_route(
    db: DatabaseGuard,
    body: Ruma<get_public_rooms::Request<'_>>,
) -> ConduitResult<get_public_rooms::Response> {
    let response = get_public_rooms_filtered_helper(
        &db,
        body.server.as_deref(),
        body.limit,
        body.since.as_deref(),
        &IncomingFilter::default(),
        &IncomingRoomNetwork::Matrix,
    )
    .await?
    .0;

    Ok(get_public_rooms::Response {
        chunk: response.chunk,
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    }
    .into())
}

/// # `PUT /_matrix/client/r0/directory/list/room/{roomId}`
///
/// Sets the visibility of a given room in the room directory.
///
/// - TODO: Access control checks
#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/directory/list/room/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn set_room_visibility_route(
    db: DatabaseGuard,
    body: Ruma<set_room_visibility::Request<'_>>,
) -> ConduitResult<set_room_visibility::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    match &body.visibility {
        room::Visibility::Public => {
            db.rooms.set_public(&body.room_id, true)?;
            info!("{} made {} public", sender_user, body.room_id);
        }
        room::Visibility::Private => db.rooms.set_public(&body.room_id, false)?,
        _ => {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Room visibility type is not supported.",
            ));
        }
    }

    db.flush()?;

    Ok(set_room_visibility::Response {}.into())
}

/// # `GET /_matrix/client/r0/directory/list/room/{roomId}`
///
/// Gets the visibility of a given room in the room directory.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/directory/list/room/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_room_visibility_route(
    db: DatabaseGuard,
    body: Ruma<get_room_visibility::Request<'_>>,
) -> ConduitResult<get_room_visibility::Response> {
    Ok(get_room_visibility::Response {
        visibility: if db.rooms.is_public_room(&body.room_id)? {
            room::Visibility::Public
        } else {
            room::Visibility::Private
        },
    }
    .into())
}

pub(crate) async fn get_public_rooms_filtered_helper(
    db: &Database,
    server: Option<&ServerName>,
    limit: Option<UInt>,
    since: Option<&str>,
    filter: &IncomingFilter,
    _network: &IncomingRoomNetwork,
) -> ConduitResult<get_public_rooms_filtered::Response> {
    if let Some(other_server) = server.filter(|server| *server != db.globals.server_name().as_str())
    {
        let response = db
            .sending
            .send_federation_request(
                &db.globals,
                other_server,
                federation::directory::get_public_rooms_filtered::v1::Request {
                    limit,
                    since: since.as_deref(),
                    filter: Filter {
                        generic_search_term: filter.generic_search_term.as_deref(),
                    },
                    room_network: RoomNetwork::Matrix,
                },
            )
            .await?;

        return Ok(get_public_rooms_filtered::Response {
            chunk: response
                .chunk
                .into_iter()
                .map(|c| {
                    // Convert ruma::api::federation::directory::get_public_rooms::v1::PublicRoomsChunk
                    // to ruma::api::client::r0::directory::PublicRoomsChunk
                    serde_json::from_str(
                        &serde_json::to_string(&c)
                            .expect("PublicRoomsChunk::to_string always works"),
                    )
                    .expect("federation and client-server PublicRoomsChunk are the same type")
                })
                .collect(),
            prev_batch: response.prev_batch,
            next_batch: response.next_batch,
            total_room_count_estimate: response.total_room_count_estimate,
        }
        .into());
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

    let mut all_rooms =
        db.rooms
            .public_rooms()
            .map(|room_id| {
                let room_id = room_id?;

                let chunk = PublicRoomsChunk {
                    aliases: Vec::new(),
                    canonical_alias: db
                        .rooms
                        .room_state_get(&room_id, &EventType::RoomCanonicalAlias, "")?
                        .map_or(Ok::<_, Error>(None), |s| {
                            Ok(serde_json::from_value::<
                                Raw<canonical_alias::CanonicalAliasEventContent>,
                            >(s.content.clone())
                            .expect("from_value::<Raw<..>> can never fail")
                            .deserialize()
                            .map_err(|_| {
                                Error::bad_database("Invalid canonical alias event in database.")
                            })?
                            .alias)
                        })?,
                    name: db
                        .rooms
                        .room_state_get(&room_id, &EventType::RoomName, "")?
                        .map_or(Ok::<_, Error>(None), |s| {
                            Ok(serde_json::from_value::<Raw<name::NameEventContent>>(
                                s.content.clone(),
                            )
                            .expect("from_value::<Raw<..>> can never fail")
                            .deserialize()
                            .map_err(|_| {
                                Error::bad_database("Invalid room name event in database.")
                            })?
                            .name)
                        })?,
                    num_joined_members: db
                        .rooms
                        .room_joined_count(&room_id)?
                        .unwrap_or_else(|| {
                            warn!("Room {} has no member count", room_id);
                            0
                        })
                        .try_into()
                        .expect("user count should not be that big"),
                    topic: db
                        .rooms
                        .room_state_get(&room_id, &EventType::RoomTopic, "")?
                        .map_or(Ok::<_, Error>(None), |s| {
                            Ok(Some(
                                serde_json::from_value::<Raw<topic::TopicEventContent>>(
                                    s.content.clone(),
                                )
                                .expect("from_value::<Raw<..>> can never fail")
                                .deserialize()
                                .map_err(|_| {
                                    Error::bad_database("Invalid room topic event in database.")
                                })?
                                .topic,
                            ))
                        })?,
                    world_readable: db
                        .rooms
                        .room_state_get(&room_id, &EventType::RoomHistoryVisibility, "")?
                        .map_or(Ok::<_, Error>(false), |s| {
                            Ok(serde_json::from_value::<
                                Raw<history_visibility::HistoryVisibilityEventContent>,
                            >(s.content.clone())
                            .expect("from_value::<Raw<..>> can never fail")
                            .deserialize()
                            .map_err(|_| {
                                Error::bad_database(
                                    "Invalid room history visibility event in database.",
                                )
                            })?
                            .history_visibility
                                == history_visibility::HistoryVisibility::WorldReadable)
                        })?,
                    guest_can_join: db
                        .rooms
                        .room_state_get(&room_id, &EventType::RoomGuestAccess, "")?
                        .map_or(Ok::<_, Error>(false), |s| {
                            Ok(
                            serde_json::from_value::<Raw<guest_access::GuestAccessEventContent>>(
                                s.content.clone(),
                            )
                            .expect("from_value::<Raw<..>> can never fail")
                            .deserialize()
                            .map_err(|_| {
                                Error::bad_database("Invalid room guest access event in database.")
                            })?
                            .guest_access
                                == guest_access::GuestAccess::CanJoin,
                        )
                        })?,
                    avatar_url: db
                        .rooms
                        .room_state_get(&room_id, &EventType::RoomAvatar, "")?
                        .map(|s| {
                            Ok::<_, Error>(
                                serde_json::from_value::<Raw<avatar::AvatarEventContent>>(
                                    s.content.clone(),
                                )
                                .expect("from_value::<Raw<..>> can never fail")
                                .deserialize()
                                .map_err(|_| {
                                    Error::bad_database("Invalid room avatar event in database.")
                                })?
                                .url,
                            )
                        })
                        .transpose()?
                        // url is now an Option<String> so we must flatten
                        .flatten(),
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
            .collect::<Vec<_>>();

    all_rooms.sort_by(|l, r| r.num_joined_members.cmp(&l.num_joined_members));

    let total_room_count_estimate = (all_rooms.len() as u32).into();

    let chunk = all_rooms
        .into_iter()
        .skip(num_since as usize)
        .take(limit as usize)
        .collect::<Vec<_>>();

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

    Ok(get_public_rooms_filtered::Response {
        chunk,
        prev_batch,
        next_batch,
        total_room_count_estimate: Some(total_room_count_estimate),
    }
    .into())
}
