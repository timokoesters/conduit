use crate::{services, Result, Ruma};
use ruma::{
    api::client::user_directory::search_users,
    events::{
        room::join_rules::{JoinRule, RoomJoinRulesEventContent},
        StateEventType,
    },
};

/// # `POST /_matrix/client/r0/user_directory/search`
///
/// Searches all known users for a match.
///
/// - Hides any local users that aren't in any public rooms (i.e. those that have the join rule set to public)
/// and don't share a room with the sender
pub async fn search_users_route(
    body: Ruma<search_users::v3::IncomingRequest>,
) -> Result<search_users::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let limit = u64::from(body.limit) as usize;

    let mut users = services().users.iter().filter_map(|user_id| {
        // Filter out buggy users (they should not exist, but you never know...)
        let user_id = user_id.ok()?;

        let user = search_users::v3::User {
            user_id: user_id.clone(),
            display_name: services().users.displayname(&user_id).ok()?,
            avatar_url: services().users.avatar_url(&user_id).ok()?,
        };

        let user_id_matches = user
            .user_id
            .to_string()
            .to_lowercase()
            .contains(&body.search_term.to_lowercase());

        let user_displayname_matches = user
            .display_name
            .as_ref()
            .filter(|name| {
                name.to_lowercase()
                    .contains(&body.search_term.to_lowercase())
            })
            .is_some();

        if !user_id_matches && !user_displayname_matches {
            return None;
        }

        let user_is_in_public_rooms = services()
            .rooms
            .state_cache
            .rooms_joined(&user_id)
            .filter_map(|r| r.ok())
            .any(|room| {
                services()
                    .rooms
                    .state_accessor
                    .room_state_get(&room, &StateEventType::RoomJoinRules, "")
                    .map_or(false, |event| {
                        event.map_or(false, |event| {
                            serde_json::from_str(event.content.get())
                                .map_or(false, |r: RoomJoinRulesEventContent| {
                                    r.join_rule == JoinRule::Public
                                })
                        })
                    })
            });

        if user_is_in_public_rooms {
            return Some(user);
        }

        let user_is_in_shared_rooms = services()
            .rooms
            .user
            .get_shared_rooms(vec![sender_user.clone(), user_id.clone()])
            .ok()?
            .next()
            .is_some();

        if user_is_in_shared_rooms {
            return Some(user);
        }

        None
    });

    let results = users.by_ref().take(limit).collect();
    let limited = users.next().is_some();

    Ok(search_users::v3::Response { results, limited })
}
