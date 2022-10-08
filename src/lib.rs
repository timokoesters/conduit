#![warn(
    rust_2018_idioms,
    unused_qualifications,
    clippy::cloned_instead_of_copied,
    clippy::str_to_string
)]
#![allow(clippy::suspicious_else_formatting)]
#![deny(clippy::dbg_macro)]

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

pub fn services<'a>() -> &'static Services {
    &SERVICES
        .read()
        .unwrap()
        .expect("SERVICES should be initialized when this is called")
}
