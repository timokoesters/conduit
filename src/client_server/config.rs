use super::State;
use crate::{ConduitResult, Database, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::config::{
            get_global_account_data, get_room_account_data, set_global_account_data,
            set_room_account_data,
        },
    },
    events::{custom::CustomEventContent, AnyBasicEventContent, BasicEvent},
    serde::Raw,
};
use serde::Deserialize;
use serde_json::value::RawValue as RawJsonValue;

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
    put(
        "/_matrix/client/r0/user/<_>/rooms/<_>/account_data/<_>",
        data = "<body>"
    )
)]
#[tracing::instrument(skip(db, body))]
pub async fn set_room_account_data_route(
    db: State<'_, Database>,
    body: Ruma<set_room_account_data::Request<'_>>,
) -> ConduitResult<set_room_account_data::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let data = serde_json::from_str(body.data.get())
        .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Data is invalid."))?;

    let event_type = body.event_type.to_string();

    db.account_data.update(
        Some(&body.room_id),
        sender_user,
        event_type.clone().into(),
        &BasicEvent {
            content: CustomEventContent { event_type, data },
        },
        &db.globals,
    )?;

    db.flush().await?;

    Ok(set_room_account_data::Response.into())
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

    let event = db
        .account_data
        .get::<Box<RawJsonValue>>(None, sender_user, body.event_type.clone().into())?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Data not found."))?;
    db.flush().await?;

    let account_data = serde_json::from_str::<ExtractEventContent>(event.get())
        .expect("event to contain a content field")
        .content;

    Ok(get_global_account_data::Response { account_data }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get(
        "/_matrix/client/r0/user/<_>/rooms/<_>/account_data/<_>",
        data = "<body>"
    )
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_room_account_data_route(
    db: State<'_, Database>,
    body: Ruma<get_room_account_data::Request<'_>>,
) -> ConduitResult<get_room_account_data::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = db
        .account_data
        .get::<Box<RawJsonValue>>(
            Some(&body.room_id),
            sender_user,
            body.event_type.clone().into(),
        )?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Data not found."))?;
    db.flush().await?;

    let account_data = serde_json::from_str::<ExtractEventContent>(event.get())
        .expect("event to contain a content field")
        .content;

    Ok(get_room_account_data::Response { account_data }.into())
}

#[derive(Deserialize)]
struct ExtractEventContent {
    content: Raw<AnyBasicEventContent>,
}
