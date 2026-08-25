use crate::{Error, Result, Ruma, services, utils};
use ruma::api::{client::typing::create_typing_event, error::ErrorKind};

/// # `PUT /_matrix/client/r0/rooms/{roomId}/typing/{userId}`
///
/// Sets the typing state of the sender user.
pub async fn create_typing_event_route(
    body: Ruma<create_typing_event::v3::Request>,
) -> Result<create_typing_event::v3::Response> {
    use create_typing_event::v3::Typing;

    let sender_user = &body.sender.user_id;

    if !services()
        .rooms
        .state_cache
        .is_joined(sender_user, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You are not in this room.",
        ));
    }

    if let Typing::Yes(typing_info) = body.state {
        services()
            .rooms
            .edus
            .typing
            .typing_add(
                sender_user,
                &body.room_id,
                typing_info.timeout.as_millis() as u64 + utils::millis_since_unix_epoch(),
            )
            .await?;
    } else {
        services()
            .rooms
            .edus
            .typing
            .typing_remove(sender_user, &body.room_id)
            .await?;
    }

    Ok(create_typing_event::v3::Response {})
}
