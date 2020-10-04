use super::State;
use crate::{utils, ConduitResult, Database, Ruma};
use ruma::api::client::r0::presence::set_presence;
use std::convert::TryInto;

#[cfg(feature = "conduit_bin")]
use rocket::put;

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/presence/<_>/status", data = "<body>")
)]
pub async fn set_presence_route(
    db: State<'_, Database<'_>>,
    body: Ruma<set_presence::Request<'_>>,
) -> ConduitResult<set_presence::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    for room_id in db.rooms.rooms_joined(&sender_user) {
        let room_id = room_id?;

        db.rooms.edus.update_presence(
            &sender_user,
            &room_id,
            ruma::events::presence::PresenceEvent {
                content: ruma::events::presence::PresenceEventContent {
                    avatar_url: db.users.avatar_url(&sender_user)?,
                    currently_active: None,
                    displayname: db.users.displayname(&sender_user)?,
                    last_active_ago: Some(
                        utils::millis_since_unix_epoch()
                            .try_into()
                            .expect("time is valid"),
                    ),
                    presence: body.presence,
                    status_msg: body.status_msg.clone(),
                },
                sender: sender_user.clone(),
            },
            &db.globals,
        )?;
    }

    db.flush().await?;

    Ok(set_presence::Response.into())
}
