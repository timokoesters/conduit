use std::{collections::HashSet, sync::Arc};

use crate::{services, Error, PduEvent, Result, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        space::{get_hierarchy, SpaceHierarchyRoomsChunk, SpaceRoomJoinRule},
    },
    events::{
        room::{
            avatar::RoomAvatarEventContent,
            canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent,
            guest_access::{GuestAccess, RoomGuestAccessEventContent},
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            name::RoomNameEventContent,
            topic::RoomTopicEventContent,
        },
        space::child::SpaceChildEventContent,
        StateEventType,
    },
    serde::Raw,
    MilliSecondsSinceUnixEpoch, OwnedRoomId, RoomId,
};
use serde_json::{self, json};
use tracing::warn;

use ruma::events::space::child::HierarchySpaceChildEvent;

/// # `GET /_matrix/client/v1/rooms/{room_id}/hierarchy``
///
/// Paginates over the space tree in a depth-first manner to locate child rooms of a given space.
///
/// - TODO: Use federation for unknown room.
///
pub async fn get_hierarchy_route(
    body: Ruma<get_hierarchy::v1::Request>,
) -> Result<get_hierarchy::v1::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    // Check if room is world readable
    let is_world_readable = services()
        .rooms
        .state_accessor
        .room_state_get(&body.room_id, &StateEventType::RoomHistoryVisibility, "")?
        .map_or(Ok(false), |s| {
            serde_json::from_str(s.content.get())
                .map(|c: RoomHistoryVisibilityEventContent| {
                    c.history_visibility == HistoryVisibility::WorldReadable
                })
                .map_err(|_| {
                    Error::bad_database("Invalid room history visibility event in database.")
                })
        })
        .unwrap_or(false);

    // Reject if user not in room and not world readable
    if !services()
        .rooms
        .state_cache
        .is_joined(sender_user, &body.room_id)?
        && !is_world_readable
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    // from format is '{suggested_only}|{max_depth}|{skip}'
    let (suggested_only, max_depth, start) = body
        .from
        .as_ref()
        .map_or(
            Some((
                body.suggested_only,
                body.max_depth
                    .map_or(services().globals.hierarchy_max_depth(), |v| v.into())
                    .min(services().globals.hierarchy_max_depth()),
                0,
            )),
            |from| {
                let mut p = from.split('|');
                Some((
                    p.next()?.trim().parse().ok()?,
                    p.next()?
                        .trim()
                        .parse::<u64>()
                        .ok()?
                        .min(services().globals.hierarchy_max_depth()),
                    p.next()?.trim().parse().ok()?,
                ))
            },
        )
        .ok_or(Error::BadRequest(ErrorKind::InvalidParam, "Invalid from"))?;

    let limit = body.limit.map_or(20u64, |v| v.into()) as usize;
    let mut skip = start;

    // Set for avoid search in loop.
    let mut room_set = HashSet::new();
    let mut rooms_chunk: Vec<SpaceHierarchyRoomsChunk> = vec![];
    let mut stack = vec![(0, body.room_id.clone())];

    while let (Some((depth, room_id)), true) = (stack.pop(), rooms_chunk.len() < limit) {
        let (childern, pdus): (Vec<_>, Vec<_>) = services()
            .rooms
            .state_accessor
            .room_state_full(&room_id)
            .await?
            .into_iter()
            .filter_map(|((e_type, key), pdu)| {
                (e_type == StateEventType::SpaceChild && !room_set.contains(&room_id))
                    .then_some((key, pdu))
            })
            .unzip();

        if skip == 0 {
            if rooms_chunk.len() < limit {
                room_set.insert(room_id.clone());
                if let Ok(chunk) = get_room_chunk(room_id, suggested_only, pdus).await {
                    rooms_chunk.push(chunk)
                };
            }
        } else {
            skip -= 1;
        }

        if depth < max_depth {
            childern.into_iter().rev().for_each(|key| {
                stack.push((depth + 1, RoomId::parse(key).unwrap()));
            });
        }
    }

    Ok(get_hierarchy::v1::Response {
        next_batch: (!stack.is_empty()).then_some(format!(
            "{}|{}|{}",
            suggested_only,
            max_depth,
            start + limit
        )),
        rooms: rooms_chunk,
    })
}

async fn get_room_chunk(
    room_id: OwnedRoomId,
    suggested_only: bool,
    pdus: Vec<Arc<PduEvent>>,
) -> Result<SpaceHierarchyRoomsChunk> {
    Ok(SpaceHierarchyRoomsChunk {
        canonical_alias: services()
            .rooms
            .state_accessor
            .room_state_get(&room_id, &StateEventType::RoomCanonicalAlias, "")
            .ok()
            .and_then(|s| {
                serde_json::from_str(s?.content.get())
                    .map(|c: RoomCanonicalAliasEventContent| c.alias)
                    .ok()?
            }),
        name: services()
            .rooms
            .state_accessor
            .room_state_get(&room_id, &StateEventType::RoomName, "")
            .ok()
            .flatten()
            .and_then(|s| {
                serde_json::from_str(s.content.get())
                    .map(|c: RoomNameEventContent| c.name)
                    .ok()?
            }),
        num_joined_members: services()
            .rooms
            .state_cache
            .room_joined_count(&room_id)?
            .unwrap_or_else(|| {
                warn!("Room {} has no member count", &room_id);
                0
            })
            .try_into()
            .expect("user count should not be that big"),
        topic: services()
            .rooms
            .state_accessor
            .room_state_get(&room_id, &StateEventType::RoomTopic, "")
            .ok()
            .and_then(|s| {
                serde_json::from_str(s?.content.get())
                    .ok()
                    .map(|c: RoomTopicEventContent| c.topic)
            }),
        world_readable: services()
            .rooms
            .state_accessor
            .room_state_get(&room_id, &StateEventType::RoomHistoryVisibility, "")?
            .map_or(Ok(false), |s| {
                serde_json::from_str(s.content.get())
                    .map(|c: RoomHistoryVisibilityEventContent| {
                        c.history_visibility == HistoryVisibility::WorldReadable
                    })
                    .map_err(|_| {
                        Error::bad_database("Invalid room history visibility event in database.")
                    })
            })?,
        guest_can_join: services()
            .rooms
            .state_accessor
            .room_state_get(&room_id, &StateEventType::RoomGuestAccess, "")?
            .map_or(Ok(false), |s| {
                serde_json::from_str(s.content.get())
                    .map(|c: RoomGuestAccessEventContent| c.guest_access == GuestAccess::CanJoin)
                    .map_err(|_| {
                        Error::bad_database("Invalid room guest access event in database.")
                    })
            })?,
        avatar_url: services()
            .rooms
            .state_accessor
            .room_state_get(&room_id, &StateEventType::RoomAvatar, "")
            .ok()
            .and_then(|s| {
                serde_json::from_str(s?.content.get())
                    .map(|c: RoomAvatarEventContent| c.url)
                    .ok()?
            }),
        join_rule: services()
            .rooms
            .state_accessor
            .room_state_get(&room_id, &StateEventType::RoomJoinRules, "")?
            .map(|s| {
                serde_json::from_str(s.content.get())
                    .map(|c: RoomJoinRulesEventContent| match c.join_rule {
                        JoinRule::Invite => SpaceRoomJoinRule::Invite,
                        JoinRule::Knock => SpaceRoomJoinRule::Knock,
                        JoinRule::KnockRestricted(_) => SpaceRoomJoinRule::KnockRestricted,
                        JoinRule::Private => SpaceRoomJoinRule::Private,
                        JoinRule::Public => SpaceRoomJoinRule::Public,
                        JoinRule::Restricted(_) => SpaceRoomJoinRule::Restricted,
                        JoinRule::_Custom(_) => SpaceRoomJoinRule::from(c.join_rule.as_str()),
                    })
                    .map_err(|_| Error::bad_database("Invalid room join rules event in database."))
            })
            .ok_or_else(|| Error::bad_database("Invalid room join rules event in database."))??,
        room_type: services()
            .rooms
            .state_accessor
            .room_state_get(&room_id, &StateEventType::RoomCreate, "")
            .map(|s| {
                serde_json::from_str(s?.content.get())
                    .map(|c: RoomCreateEventContent| c.room_type)
                    .ok()?
            })
            .ok()
            .flatten(),
        children_state: pdus
            .into_iter()
            .flat_map(|pdu| {
                Some(HierarchySpaceChildEvent {
                    // Ignore unsuggested rooms if suggested_only is set
                    content: serde_json::from_str(pdu.content.get()).ok().filter(
                        |pdu: &SpaceChildEventContent| {
                            !suggested_only || pdu.suggested.unwrap_or(false)
                        },
                    )?,
                    sender: pdu.sender.clone(),
                    state_key: pdu.state_key.clone()?,
                    origin_server_ts: MilliSecondsSinceUnixEpoch(pdu.origin_server_ts),
                })
            })
            .filter_map(|hsce| {
                Raw::<HierarchySpaceChildEvent>::from_json_string(
                    json!(
                     {
                      "content": &hsce.content,
                      "sender": &hsce.sender,
                      "state_key": &hsce.state_key,
                      "origin_server_ts": &hsce.origin_server_ts
                     }
                    )
                    .to_string(),
                )
                .ok()
            })
            .collect::<Vec<_>>(),
        room_id,
    })
}
