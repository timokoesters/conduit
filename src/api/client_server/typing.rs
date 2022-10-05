use crate::{services, utils, Error, Result, Ruma};
use ruma::api::client::{error::ErrorKind, typing::create_typing_event};

/// # `PUT /_matrix/client/r0/rooms/{roomId}/typing/{userId}`
///
/// Sets the typing state of the sender user.
pub async fn create_typing_event_route(
    body: Ruma<create_typing_event::v3::IncomingRequest>,
) -> Result<create_typing_event::v3::Response> {
    use create_typing_event::v3::Typing;

    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

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

    if let Typing::Yes(duration) = body.state {
        services().rooms.edus.typing.typing_add(
            sender_user,
            &body.room_id,
            duration.as_millis() as u64 + utils::millis_since_unix_epoch(),
        )?;
    } else {
        services()
            .rooms
            .edus
            .typing
            .typing_remove(sender_user, &body.room_id)?;
    }

    Ok(create_typing_event::v3::Response {})
}
