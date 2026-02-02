use crate::{Result, Ruma};
use ruma::api::client::thirdparty::get_protocols;

use std::collections::BTreeMap;

/// # `GET /_matrix/client/r0/thirdparty/protocols`
///
/// TODO: Fetches all metadata about protocols supported by the homeserver.
pub async fn get_protocols_route(
    _body: Ruma<get_protocols::v3::Request>,
) -> Result<get_protocols::v3::Response> {
    // TODO
    Ok(get_protocols::v3::Response {
        protocols: BTreeMap::new(),
    })
}
