use super::State;
use crate::{server_server, ConduitResult, Database, Error, Result, Ruma};
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
    directory::PublicRoomsChunk,
    events::{
        room::{avatar, canonical_alias, guest_access, history_visibility, name, topic},
        EventType,
    },
    Raw,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post, put};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/publicRooms", data = "<body>")
)]
pub async fn get_public_rooms_filtered_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms_filtered::IncomingRequest>,
) -> ConduitResult<get_public_rooms_filtered::Response> {
    if let Some(other_server) = body
        .server
        .clone()
        .filter(|server| server != db.globals.server_name().as_str())
    {
        let response = server_server::send_request(
            &db,
            other_server,
            federation::directory::get_public_rooms::v1::Request {
                limit: body.limit,
                since: body.since.as_deref(),
                room_network: ruma::directory::RoomNetwork::Matrix,
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
                    Ok::<_, Error>(
                        serde_json::from_str(
                            &serde_json::to_string(&c)
                                .expect("PublicRoomsChunk::to_string always works"),
                        )
                        .expect("federation and client-server PublicRoomsChunk are the same type"),
                    )
                })
                .filter_map(|r| r.ok())
                .collect(),
            prev_batch: response.prev_batch,
            next_batch: response.next_batch,
            total_room_count_estimate: response.total_room_count_estimate,
        }
        .into());
    }

    let limit = body.limit.map_or(10, u64::from);
    let mut since = 0_u64;

    if let Some(s) = &body.since {
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

        since = characters
            .collect::<String>()
            .parse()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid `since` token."))?;

        if backwards {
            since = since.saturating_sub(limit);
        }
    }

    let mut all_rooms =
        db.rooms
            .public_rooms()
            .map(|room_id| {
                let room_id = room_id?;

                // TODO: Do not load full state?
                let state = db.rooms.room_state_full(&room_id)?;

                let chunk = PublicRoomsChunk {
                    aliases: Vec::new(),
                    canonical_alias: state
                        .get(&(EventType::RoomCanonicalAlias, "".to_owned()))
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
                    name: state.get(&(EventType::RoomName, "".to_owned())).map_or(
                        Ok::<_, Error>(None),
                        |s| {
                            Ok(serde_json::from_value::<Raw<name::NameEventContent>>(
                                s.content.clone(),
                            )
                            .expect("from_value::<Raw<..>> can never fail")
                            .deserialize()
                            .map_err(|_| {
                                Error::bad_database("Invalid room name event in database.")
                            })?
                            .name()
                            .map(|n| n.to_owned()))
                        },
                    )?,
                    num_joined_members: (db.rooms.room_members(&room_id).count() as u32).into(),
                    room_id,
                    topic: state.get(&(EventType::RoomTopic, "".to_owned())).map_or(
                        Ok::<_, Error>(None),
                        |s| {
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
                        },
                    )?,
                    world_readable: state
                        .get(&(EventType::RoomHistoryVisibility, "".to_owned()))
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
                    guest_can_join: state
                        .get(&(EventType::RoomGuestAccess, "".to_owned()))
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
                    avatar_url: state
                        .get(&(EventType::RoomAvatar, "".to_owned()))
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
                        .transpose()?,
                };
                Ok(chunk)
            })
            .filter_map(|r: Result<_>| r.ok()) // Filter out buggy rooms
            // We need to collect all, so we can sort by member count
            .collect::<Vec<_>>();

    all_rooms.sort_by(|l, r| r.num_joined_members.cmp(&l.num_joined_members));

    let total_room_count_estimate = (all_rooms.len() as u32).into();

    let chunk = all_rooms
        .into_iter()
        .skip(since as usize)
        .take(limit as usize)
        .collect::<Vec<_>>();

    let prev_batch = if since == 0 {
        None
    } else {
        Some(format!("p{}", since))
    };

    let next_batch = if chunk.len() < limit as usize {
        None
    } else {
        Some(format!("n{}", since + limit))
    };

    Ok(get_public_rooms_filtered::Response {
        chunk,
        prev_batch,
        next_batch,
        total_room_count_estimate: Some(total_room_count_estimate),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/publicRooms", data = "<body>")
)]
pub async fn get_public_rooms_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms::IncomingRequest>,
) -> ConduitResult<get_public_rooms::Response> {
    let Ruma {
        body:
            get_public_rooms::IncomingRequest {
                limit,
                server,
                since,
            },
        sender_id,
        device_id,
        json_body,
    } = body;

    let get_public_rooms_filtered::Response {
        chunk,
        prev_batch,
        next_batch,
        total_room_count_estimate,
    } = get_public_rooms_filtered_route(
        db,
        Ruma {
            body: get_public_rooms_filtered::IncomingRequest {
                filter: None,
                limit,
                room_network: ruma::directory::RoomNetwork::Matrix,
                server,
                since,
            },
            sender_id,
            device_id,
            json_body,
        },
    )
    .await?
    .0;

    Ok(get_public_rooms::Response {
        chunk,
        prev_batch,
        next_batch,
        total_room_count_estimate,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/directory/list/room/<_>", data = "<body>")
)]
pub async fn set_room_visibility_route(
    db: State<'_, Database>,
    body: Ruma<set_room_visibility::Request>,
) -> ConduitResult<set_room_visibility::Response> {
    match body.visibility {
        room::Visibility::Public => db.rooms.set_public(&body.room_id, true)?,
        room::Visibility::Private => db.rooms.set_public(&body.room_id, false)?,
    }

    Ok(set_room_visibility::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/directory/list/room/<_>", data = "<body>")
)]
pub async fn get_room_visibility_route(
    db: State<'_, Database>,
    body: Ruma<get_room_visibility::Request>,
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
