use crate::ConduitResult;
use ruma::{
    api::client::r0::capabilities::get_capabilities::{self, ChangePasswordCapability},
    RoomVersionId,
};
use std::collections::BTreeMap;

#[cfg(feature = "conduit_bin")]
use rocket::get;

/// # `GET /_matrix/client/r0/capabilities`
///
/// Get information on this server's supported feature set and other relevent capabilities.
#[cfg_attr(feature = "conduit_bin", get("/_matrix/client/r0/capabilities"))]
pub async fn get_capabilities_route() -> ConduitResult<get_capabilities::Response> {
    let mut available = BTreeMap::new();
    available.insert(
        RoomVersionId::Version5,
        get_capabilities::RoomVersionStability::Stable,
    );
    available.insert(
        RoomVersionId::Version6,
        get_capabilities::RoomVersionStability::Stable,
    );

    Ok(get_capabilities::Response {
        capabilities: get_capabilities::Capabilities {
            change_password: ChangePasswordCapability { enabled: true },
            room_versions: get_capabilities::RoomVersionsCapability {
                default: RoomVersionId::Version6,
                available,
            },
            custom_capabilities: BTreeMap::new(),
        },
    }
    .into())
}
