use crate::{database::DatabaseGuard, ConduitResult, Ruma};
use ruma::api::client::r0::capabilities::{
    get_capabilities, Capabilities, RoomVersionStability, RoomVersionsCapability,
};
use std::collections::BTreeMap;

#[cfg(feature = "conduit_bin")]
use rocket::get;

/// # `GET /_matrix/client/r0/capabilities`
///
/// Get information on the supported feature set and other relevent capabilities of this server.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/capabilities", data = "<_body>")
)]
#[tracing::instrument(skip(_body, db))]
pub async fn get_capabilities_route(
    _body: Ruma<get_capabilities::Request>,
    db: DatabaseGuard,
) -> ConduitResult<get_capabilities::Response> {
    let mut available = BTreeMap::new();
    if db.globals.allow_unstable_room_versions() {
        for room_version in &db.globals.unstable_room_versions {
            available.insert(room_version.clone(), RoomVersionStability::Stable);
        }
    } else {
        for room_version in &db.globals.unstable_room_versions {
            available.insert(room_version.clone(), RoomVersionStability::Unstable);
        }
    }
    for room_version in &db.globals.stable_room_versions {
        available.insert(room_version.clone(), RoomVersionStability::Stable);
    }

    let mut capabilities = Capabilities::new();
    capabilities.room_versions = RoomVersionsCapability {
        default: db.globals.default_room_version(),
        available,
    };

    Ok(get_capabilities::Response { capabilities }.into())
}
