// All API endpoints must be async
#[allow(clippy::unused_async)]
// We expect request users and servers (probably shouldn't tho)
#[allow(clippy::missing_panics_doc)]
pub mod api;
pub mod clap;
mod config;
// Results in large capacity if set to a negative number, user's fault really :P
#[allow(clippy::cast_sign_loss)]
mod database;
// `self` is required for easy access to methods
#[allow(clippy::unused_self)]
mod service;
mod utils;

// Not async due to services() being used in many closures, and async closures are not stable as of writing
// This is the case for every other occurence of sync Mutex/RwLock, except for database related ones, where
// the current maintainer (Timo) has asked to not modify those
use std::sync::RwLock;

pub use api::ruma_wrapper::{Ruma, RumaResponse};
pub use config::Config;
pub use database::KeyValueDatabase;
pub use service::{pdu::PduEvent, Services};
pub use utils::error::{Error, Result};

pub static SERVICES: RwLock<Option<&'static Services>> = RwLock::new(None);

pub fn services() -> &'static Services {
    SERVICES
        .read()
        .unwrap()
        .expect("SERVICES should be initialized when this is called")
}
