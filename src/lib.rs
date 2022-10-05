#![warn(
    rust_2018_idioms,
    unused_qualifications,
    clippy::cloned_instead_of_copied,
    clippy::str_to_string
)]
#![allow(clippy::suspicious_else_formatting)]
#![deny(clippy::dbg_macro)]

mod config;
mod database;
mod service;
pub mod api;
mod utils;

use std::{cell::Cell, sync::{RwLock, Arc}};

pub use config::Config;
pub use utils::error::{Error, Result};
pub use service::{Services, pdu::PduEvent};
pub use api::ruma_wrapper::{Ruma, RumaResponse};

pub static SERVICES: RwLock<Option<&'static Services>> = RwLock::new(None);

pub fn services<'a>() -> &'static Services {
    &SERVICES.read().unwrap().expect("SERVICES should be initialized when this is called")
}

