use crate::{services, Error, Result, Ruma};
use rand::seq::SliceRandom;
use ruma::{
    api::{
        appservice,
        client::{
            alias::{create_alias, delete_alias, get_alias},
            error::ErrorKind,
        },
        federation,
    },
    OwnedRoomAliasId,
};

/// # `PUT /_matrix/client/r0/directory/room/{roomAlias}`
///
/// Creates a new room alias on this server.
pub async fn create_alias_route(
    body: Ruma<create_alias::v3::Request>,
) -> Result<create_alias::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if body.room_alias.server_name() != services().globals.server_name() {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Alias is from another server.",
        ));
    }

    if let Some(ref info) = body.appservice_info {
        if !info.aliases.is_match(body.room_alias.as_str()) {
            return Err(Error::BadRequest(
                ErrorKind::Exclusive,
                "Room alias is not in namespace.",
            ));
        }
    } else if services()
        .appservice
        .is_exclusive_alias(&body.room_alias)
        .await
    {
        return Err(Error::BadRequest(
            ErrorKind::Exclusive,
            "Room alias reserved by appservice.",
        ));
    }

    if services()
        .rooms
        .alias
        .resolve_local_alias(&body.room_alias)?
        .is_some()
    {
        return Err(Error::Conflict("Alias already exists."));
    }

    services()
        .rooms
        .alias
        .set_alias(&body.room_alias, &body.room_id, sender_user)?;

    Ok(create_alias::v3::Response::new())
}

/// # `DELETE /_matrix/client/r0/directory/room/{roomAlias}`
///
/// Deletes a room alias from this server.
///
/// - TODO: Update canonical alias event
pub async fn delete_alias_route(
    body: Ruma<delete_alias::v3::Request>,
) -> Result<delete_alias::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if body.room_alias.server_name() != services().globals.server_name() {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Alias is from another server.",
        ));
    }

    if let Some(ref info) = body.appservice_info {
        if !info.aliases.is_match(body.room_alias.as_str()) {
            return Err(Error::BadRequest(
                ErrorKind::Exclusive,
                "Room alias is not in namespace.",
            ));
        }
    } else if services()
        .appservice
        .is_exclusive_alias(&body.room_alias)
        .await
    {
        return Err(Error::BadRequest(
            ErrorKind::Exclusive,
            "Room alias reserved by appservice.",
        ));
    }

    services()
        .rooms
        .alias
        .remove_alias(&body.room_alias, sender_user)?;

    // TODO: update alt_aliases?

    Ok(delete_alias::v3::Response::new())
}

/// # `GET /_matrix/client/r0/directory/room/{roomAlias}`
///
/// Resolve an alias locally or over federation.
///
/// - TODO: Suggest more servers to join via
pub async fn get_alias_route(
    body: Ruma<get_alias::v3::Request>,
) -> Result<get_alias::v3::Response> {
    get_alias_helper(body.body.room_alias).await
}

pub(crate) async fn get_alias_helper(
    room_alias: OwnedRoomAliasId,
) -> Result<get_alias::v3::Response> {
    if room_alias.server_name() != services().globals.server_name() {
        let response = services()
            .sending
            .send_federation_request(
                room_alias.server_name(),
                federation::query::get_room_information::v1::Request {
                    room_alias: room_alias.to_owned(),
                },
            )
            .await?;

        let mut servers = response.servers;
        servers.shuffle(&mut rand::thread_rng());

        return Ok(get_alias::v3::Response::new(response.room_id, servers));
    }

    let mut room_id = None;
    match services().rooms.alias.resolve_local_alias(&room_alias)? {
        Some(r) => room_id = Some(r),
        None => {
            for appservice in services().appservice.read().await.values() {
                if appservice.aliases.is_match(room_alias.as_str())
                    && matches!(
                        services()
                            .sending
                            .send_appservice_request(
                                appservice.registration.clone(),
                                appservice::query::query_room_alias::v1::Request {
                                    room_alias: room_alias.clone(),
                                },
                            )
                            .await,
                        Ok(Some(_opt_result))
                    )
                {
                    room_id = Some(
                        services()
                            .rooms
                            .alias
                            .resolve_local_alias(&room_alias)?
                            .ok_or_else(|| {
                                Error::bad_config("Appservice lied to us. Room does not exist.")
                            })?,
                    );
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

    Ok(get_alias::v3::Response::new(
        room_id,
        vec![services().globals.server_name().to_owned()],
    ))
}
