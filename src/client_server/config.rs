use super::State;
use crate::{ConduitResult, Database, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::config::{get_global_account_data, set_global_account_data},
    },
    events::{custom::CustomEventContent, BasicEvent},
    Raw,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, put};

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/user/<_>/account_data/<_>", data = "<body>")
)]
pub fn set_global_account_data_route(
    db: State<'_, Database>,
    body: Ruma<set_global_account_data::Request>,
) -> ConduitResult<set_global_account_data::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    let content = serde_json::from_str::<serde_json::Value>(body.data.get())
        .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Data is invalid."))?;

    let event_type = body.event_type.to_string();

    db.account_data.update(
        None,
        sender_id,
        event_type.clone().into(),
        &BasicEvent {
            content: CustomEventContent {
                event_type,
                json: content,
            },
        },
        &db.globals,
    )?;

    Ok(set_global_account_data::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/user/<_>/account_data/<_>", data = "<body>")
)]
pub fn get_global_account_data_route(
    db: State<'_, Database>,
    body: Ruma<get_global_account_data::Request>,
) -> ConduitResult<get_global_account_data::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    let data = db
        .account_data
        .get::<Raw<ruma::events::AnyBasicEvent>>(None, sender_id, body.event_type.clone().into())?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Data not found."))?;

    Ok(get_global_account_data::Response { account_data: data }.into())
}
