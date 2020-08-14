use super::State;
use crate::{server_server, ConduitResult, Database, Error, Ruma};
use ruma::api::{
    client::{
        error::ErrorKind,
        r0::alias::{create_alias, delete_alias, get_alias},
    },
    federation,
};

#[cfg(feature = "conduit_bin")]
use rocket::{delete, get, put};

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/directory/room/<_>", data = "<body>")
)]
pub fn create_alias_route(
    db: State<'_, Database>,
    body: Ruma<create_alias::IncomingRequest>,
) -> ConduitResult<create_alias::Response> {
    if db.rooms.id_from_alias(&body.room_alias)?.is_some() {
        return Err(Error::Conflict("Alias already exists."));
    }

    db.rooms
        .set_alias(&body.room_alias, Some(&body.room_id), &db.globals)?;

    Ok(create_alias::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    delete("/_matrix/client/r0/directory/room/<_>", data = "<body>")
)]
pub fn delete_alias_route(
    db: State<'_, Database>,
    body: Ruma<delete_alias::IncomingRequest>,
) -> ConduitResult<delete_alias::Response> {
    db.rooms.set_alias(&body.room_alias, None, &db.globals)?;

    Ok(delete_alias::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/directory/room/<_>", data = "<body>")
)]
pub async fn get_alias_route(
    db: State<'_, Database>,
    body: Ruma<get_alias::IncomingRequest>,
) -> ConduitResult<get_alias::Response> {
    if body.room_alias.server_name() != db.globals.server_name() {
        let response = server_server::send_request(
            &db,
            body.room_alias.server_name().to_string(),
            federation::query::get_room_information::v1::Request {
                room_alias: body.room_alias.to_string(),
            },
        )
        .await?;

        return Ok(get_alias::Response {
            room_id: response.room_id,
            servers: response.servers,
        }
        .into());
    }

    let room_id = db
        .rooms
        .id_from_alias(&body.room_alias)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Room with alias not found.",
        ))?;

    Ok(get_alias::Response {
        room_id,
        servers: vec![db.globals.server_name().to_string()],
    }
    .into())
}
