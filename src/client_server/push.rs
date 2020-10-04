use super::State;
use crate::{ConduitResult, Database, Error, Ruma};
use log::warn;
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::push::{get_pushers, get_pushrules_all, set_pushrule, set_pushrule_enabled},
    },
    events::EventType,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post, put};

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushrules", data = "<body>")
)]
pub fn get_pushrules_all_route(
    db: State<'_, Database<'_>>,
    body: Ruma<get_pushrules_all::Request>,
) -> ConduitResult<get_pushrules_all::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    let event = db
        .account_data
        .get::<ruma::events::push_rules::PushRulesEvent>(None, &sender_id, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    Ok(get_pushrules_all::Response {
        global: event.content.global,
    }
    .into())
}

#[cfg_attr(feature = "conduit_bin", put(
    "/_matrix/client/r0/pushrules/<_>/<_>/<_>",
    //data = "<body>"
))]
pub fn set_pushrule_route(//db: State<'_, Database<'_>>,
    //body: Ruma<set_pushrule::Request>,
) -> ConduitResult<set_pushrule::Response> {
    // TODO
    warn!("TODO: set_pushrule_route");
    Ok(set_pushrule::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/pushrules/<_>/<_>/<_>/enabled")
)]
pub fn set_pushrule_enabled_route() -> ConduitResult<set_pushrule_enabled::Response> {
    // TODO
    warn!("TODO: set_pushrule_enabled_route");
    Ok(set_pushrule_enabled::Response.into())
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/client/r0/pushers"))]
pub fn get_pushers_route() -> ConduitResult<get_pushers::Response> {
    Ok(get_pushers::Response {
        pushers: Vec::new(),
    }
    .into())
}

#[cfg_attr(feature = "conduit_bin", post("/_matrix/client/r0/pushers/set"))]
pub fn set_pushers_route() -> ConduitResult<get_pushers::Response> {
    Ok(get_pushers::Response {
        pushers: Vec::new(),
    }
    .into())
}
