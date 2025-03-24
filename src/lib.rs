pub mod api;
pub mod clap;
mod config;
mod database;
mod service;
mod utils;

// Not async due to services() being used in many closures, and async closures are not stable as of writing
// This is the case for every other occurrence of sync Mutex/RwLock, except for database related ones, where
// the current maintainer (Timo) has asked to not modify those
use std::sync::RwLock;

pub use api::ruma_wrapper::{Ruma, RumaResponse};
pub use config::Config;
pub use database::KeyValueDatabase;
use ruma::api::MatrixVersion;
pub use service::{pdu::PduEvent, Services};
pub use utils::error::{Error, Result};

pub static SERVICES: RwLock<Option<&'static Services>> = RwLock::new(None);
pub const MATRIX_VERSIONS: &[MatrixVersion] = &[MatrixVersion::V1_13];

pub fn services() -> &'static Services {
    SERVICES
        .read()
        .unwrap()
        .expect("SERVICES should be initialized when this is called")
}
