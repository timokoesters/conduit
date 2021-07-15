mod account;
mod alias;
mod backup;
mod capabilities;
mod config;
mod context;
mod device;
mod directory;
mod filter;
mod keys;
mod media;
mod membership;
mod message;
mod presence;
mod profile;
mod push;
mod read_marker;
mod redact;
mod room;
mod search;
mod session;
mod state;
mod sync;
mod tag;
mod thirdparty;
mod to_device;
mod typing;
mod unversioned;
mod user_directory;
mod voip;

pub use account::*;
pub use alias::*;
pub use backup::*;
pub use capabilities::*;
pub use config::*;
pub use context::*;
pub use device::*;
pub use directory::*;
pub use filter::*;
pub use keys::*;
pub use media::*;
pub use membership::*;
pub use message::*;
pub use presence::*;
pub use profile::*;
pub use push::*;
pub use read_marker::*;
pub use redact::*;
pub use room::*;
pub use search::*;
pub use session::*;
pub use state::*;
pub use sync::*;
pub use tag::*;
pub use thirdparty::*;
pub use to_device::*;
pub use typing::*;
pub use unversioned::*;
pub use user_directory::*;
pub use voip::*;

#[cfg(not(feature = "conduit_bin"))]
use super::State;
#[cfg(feature = "conduit_bin")]
use {
    crate::ConduitResult, rocket::options, ruma::api::client::r0::to_device::send_event_to_device,
};

pub const DEVICE_ID_LENGTH: usize = 10;
pub const TOKEN_LENGTH: usize = 256;
pub const SESSION_ID_LENGTH: usize = 256;

#[cfg(feature = "conduit_bin")]
#[options("/<_..>")]
#[tracing::instrument]
pub async fn options_route() -> ConduitResult<send_event_to_device::Response> {
    Ok(send_event_to_device::Response {}.into())
}
