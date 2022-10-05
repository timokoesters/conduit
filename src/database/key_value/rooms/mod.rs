mod alias;
mod auth_chain;
mod directory;
mod edus;
mod lazy_load;
mod metadata;
mod outlier;
mod pdu_metadata;
mod search;
mod short;
mod state;
mod state_accessor;
mod state_cache;
mod state_compressor;
mod timeline;
mod user;

use std::sync::Arc;

use crate::{database::KeyValueDatabase, service};

impl service::rooms::Data for Arc<KeyValueDatabase> {}
