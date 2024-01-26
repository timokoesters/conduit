pub mod api;
mod config;
mod database;
mod service;
mod utils;

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
