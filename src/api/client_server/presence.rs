use crate::{services, Result, Ruma};
use ruma::{
    api::client::presence::{get_presence, set_presence},
    presence::PresenceState,
    uint,
};
use std::time::Duration;

/// # `PUT /_matrix/client/r0/presence/{userId}/status`
///
/// Sets the presence state of the sender user.
pub async fn set_presence_route(
    body: Ruma<set_presence::v3::IncomingRequest>,
) -> Result<set_presence::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    for room_id in services().rooms.state_cache.rooms_joined(sender_user) {
        let room_id = room_id?;

        services().rooms.edus.presence.update_presence(
            sender_user,
            &room_id,
            ruma::events::presence::PresenceEvent {
                content: ruma::events::presence::PresenceEventContent {
                    avatar_url: services().users.avatar_url(sender_user)?,
                    currently_active: None,
                    displayname: services().users.displayname(sender_user)?,
                    last_active_ago: Some(uint!(0)),
                    presence: body.presence.clone(),
                    status_msg: body.status_msg.clone(),
                },
                sender: sender_user.clone(),
            },
            true,
        )?;
    }

    Ok(set_presence::v3::Response {})
}

/// # `GET /_matrix/client/r0/presence/{userId}/status`
///
/// Gets the presence state of the given user.
///
/// - Only works if you share a room with the user
pub async fn get_presence_route(
    body: Ruma<get_presence::v3::IncomingRequest>,
) -> Result<get_presence::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut presence_event = None;

    for room_id in services()
        .rooms
        .user
        .get_shared_rooms(vec![sender_user.clone(), body.user_id.clone()])?
    {
        let room_id = room_id?;

        if let Some(presence) = services()
            .rooms
            .edus
            .presence
            .get_presence_event(sender_user, &room_id)?
        {
            presence_event = Some(presence);
            break;
        }
    }

    if let Some(presence) = presence_event {
        Ok(get_presence::v3::Response {
            status_msg: presence.content.status_msg,
            currently_active: presence.content.currently_active,
            last_active_ago: presence
                .content
                .last_active_ago
                .map(|millis| Duration::from_millis(millis.into())),
            presence: presence.content.presence,
        })
    } else {
        Ok(get_presence::v3::Response {
            status_msg: None,
            currently_active: None,
            last_active_ago: None,
            presence: PresenceState::Offline,
        })
    }
}
