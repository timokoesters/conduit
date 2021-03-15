use super::State;
use crate::{ConduitResult, Database, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::config::{get_global_account_data, set_global_account_data},
    },
    events::{custom::CustomEventContent, BasicEvent},
    serde::Raw,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, put};

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/user/<_>/account_data/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn set_global_account_data_route(
    db: State<'_, Database>,
    body: Ruma<set_global_account_data::Request<'_>>,
) -> ConduitResult<set_global_account_data::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let data = serde_json::from_str(body.data.get())
        .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Data is invalid."))?;

    let event_type = body.event_type.to_string();

    db.account_data.update(
        None,
        sender_user,
        event_type.clone().into(),
        &BasicEvent {
            content: CustomEventContent { event_type, data },
        },
        &db.globals,
    )?;

    db.flush().await?;

    Ok(set_global_account_data::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/user/<_>/account_data/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_global_account_data_route(
    db: State<'_, Database>,
    body: Ruma<get_global_account_data::Request<'_>>,
) -> ConduitResult<get_global_account_data::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let data = db
        .account_data
        .get::<Raw<ruma::events::AnyBasicEvent>>(None, sender_user, body.event_type.clone().into())?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Data not found."))?;

    db.flush().await?;

    Ok(get_global_account_data::Response { account_data: data }.into())
}
