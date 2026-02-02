use crate::{services, Error, Result, Ruma};
use ruma::api::client::{
    alias::{create_alias, delete_alias, get_alias},
    error::ErrorKind,
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
    services()
        .rooms
        .alias
        .get_alias_helper(body.body.room_alias)
        .await
}
