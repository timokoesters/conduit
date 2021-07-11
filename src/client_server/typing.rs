use crate::{database::DatabaseGuard, utils, ConduitResult, Ruma};
use create_typing_event::Typing;
use ruma::api::client::r0::typing::create_typing_event;

#[cfg(feature = "conduit_bin")]
use rocket::put;

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/rooms/<_>/typing/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn create_typing_event_route(
    db: DatabaseGuard,
    body: Ruma<create_typing_event::Request<'_>>,
) -> ConduitResult<create_typing_event::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if let Typing::Yes(duration) = body.state {
        db.rooms.edus.typing_add(
            &sender_user,
            &body.room_id,
            duration.as_millis() as u64 + utils::millis_since_unix_epoch(),
            &db.globals,
        )?;
    } else {
        db.rooms
            .edus
            .typing_remove(&sender_user, &body.room_id, &db.globals)?;
    }

    Ok(create_typing_event::Response.into())
}
