use super::State;
use crate::{server_server, ConduitResult, Database, Error, Ruma};
use ruma::{
    api::{
        client::{
            error::ErrorKind,
            r0::alias::{create_alias, delete_alias, get_alias},
        },
        federation,
    },
    RoomAliasId,
};

#[cfg(feature = "conduit_bin")]
use rocket::{delete, get, put};

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/directory/room/<_>", data = "<body>")
)]
pub async fn create_alias_route(
    db: State<'_, Database<'_>>,
    body: Ruma<create_alias::Request<'_>>,
) -> ConduitResult<create_alias::Response> {
    if db.rooms.id_from_alias(&body.room_alias)?.is_some() {
        return Err(Error::Conflict("Alias already exists."));
    }

    db.rooms
        .set_alias(&body.room_alias, Some(&body.room_id), &db.globals)?;

    db.flush().await?;

    Ok(create_alias::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    delete("/_matrix/client/r0/directory/room/<_>", data = "<body>")
)]
pub async fn delete_alias_route(
    db: State<'_, Database<'_>>,
    body: Ruma<delete_alias::Request<'_>>,
) -> ConduitResult<delete_alias::Response> {
    db.rooms.set_alias(&body.room_alias, None, &db.globals)?;

    db.flush().await?;

    Ok(delete_alias::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/directory/room/<_>", data = "<body>")
)]
pub async fn get_alias_route(
    db: State<'_, Database<'_>>,
    body: Ruma<get_alias::Request<'_>>,
) -> ConduitResult<get_alias::Response> {
    get_alias_helper(&db, &body.room_alias).await
}

pub async fn get_alias_helper(
    db: &Database<'static>,
    room_alias: &RoomAliasId,
) -> ConduitResult<get_alias::Response> {
    if room_alias.server_name() != db.globals.server_name() {
        let response = server_server::send_request(
            &db.globals,
            room_alias.server_name().to_owned(),
            federation::query::get_room_information::v1::Request { room_alias },
        )
        .await?;

        return Ok(get_alias::Response::new(response.room_id, response.servers).into());
    }

    let room_id = db
        .rooms
        .id_from_alias(&room_alias)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Room with alias not found.",
        ))?;

    Ok(get_alias::Response::new(room_id, vec![db.globals.server_name().to_owned()]).into())
}
