use super::State;
use crate::{ConduitResult, Database, Error, Ruma};
use regex::Regex;
use ruma::{
    api::{
        appservice,
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
#[tracing::instrument(skip(db, body))]
pub async fn create_alias_route(
    db: State<'_, Database>,
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
#[tracing::instrument(skip(db, body))]
pub async fn delete_alias_route(
    db: State<'_, Database>,
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
#[tracing::instrument(skip(db, body))]
pub async fn get_alias_route(
    db: State<'_, Database>,
    body: Ruma<get_alias::Request<'_>>,
) -> ConduitResult<get_alias::Response> {
    get_alias_helper(&db, &body.room_alias).await
}

pub async fn get_alias_helper(
    db: &Database,
    room_alias: &RoomAliasId,
) -> ConduitResult<get_alias::Response> {
    if room_alias.server_name() != db.globals.server_name() {
        let response = db
            .sending
            .send_federation_request(
                &db.globals,
                room_alias.server_name(),
                federation::query::get_room_information::v1::Request { room_alias },
            )
            .await?;

        return Ok(get_alias::Response::new(response.room_id, response.servers).into());
    }

    let mut room_id = None;
    match db.rooms.id_from_alias(&room_alias)? {
        Some(r) => room_id = Some(r),
        None => {
            for (_id, registration) in db.appservice.iter_all().filter_map(|r| r.ok()) {
                let aliases = registration
                    .get("namespaces")
                    .and_then(|ns| ns.get("aliases"))
                    .and_then(|users| users.get("regex"))
                    .and_then(|regex| regex.as_str())
                    .and_then(|regex| Regex::new(regex).ok());

                if aliases.map_or(false, |aliases| aliases.is_match(room_alias.as_str()))
                    && db
                        .sending
                        .send_appservice_request(
                            &db.globals,
                            registration,
                            appservice::query::query_room_alias::v1::Request { room_alias },
                        )
                        .await
                        .is_ok()
                {
                    room_id = Some(db.rooms.id_from_alias(&room_alias)?.ok_or_else(|| {
                        Error::bad_config("Appservice lied to us. Room does not exist.")
                    })?);
                    break;
                }
            }
        }
    };

    let room_id = match room_id {
        Some(room_id) => room_id,
        None => {
            return Err(Error::BadRequest(
                ErrorKind::NotFound,
                "Room with alias not found.",
            ))
        }
    };

    Ok(get_alias::Response::new(room_id, vec![db.globals.server_name().to_owned()]).into())
}
