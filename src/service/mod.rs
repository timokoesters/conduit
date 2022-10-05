use std::{
    collections::{HashMap},
    sync::{Arc, Mutex},
};

use lru_cache::LruCache;

use crate::{Config, Result};

pub mod account_data;
pub mod admin;
pub mod appservice;
pub mod globals;
pub mod key_backups;
pub mod media;
pub mod pdu;
pub mod pusher;
pub mod rooms;
pub mod sending;
pub mod transaction_ids;
pub mod uiaa;
pub mod users;

pub struct Services {
    pub appservice: appservice::Service,
    pub pusher: pusher::Service,
    pub rooms: rooms::Service,
    pub transaction_ids: transaction_ids::Service,
    pub uiaa: uiaa::Service,
    pub users: users::Service,
    pub account_data: account_data::Service,
    pub admin: admin::Service,
    pub globals: globals::Service,
    pub key_backups: key_backups::Service,
    pub media: media::Service,
    pub sending: sending::Service,
}

impl Services {
    pub fn build<
        D: appservice::Data
            + pusher::Data
            + rooms::Data
            + transaction_ids::Data
            + uiaa::Data
            + users::Data
            + account_data::Data
            + globals::Data
            + key_backups::Data
            + media::Data,
    >(
        db: Arc<D>,
        config: Config,
    ) -> Result<Self> {
        Ok(Self {
            appservice: appservice::Service { db: db.clone() },
            pusher: pusher::Service { db: db.clone() },
            rooms: rooms::Service {
                alias: rooms::alias::Service { db: db.clone() },
                auth_chain: rooms::auth_chain::Service { db: db.clone() },
                directory: rooms::directory::Service { db: db.clone() },
                edus: rooms::edus::Service {
                    presence: rooms::edus::presence::Service { db: db.clone() },
                    read_receipt: rooms::edus::read_receipt::Service { db: db.clone() },
                    typing: rooms::edus::typing::Service { db: db.clone() },
                },
                event_handler: rooms::event_handler::Service,
                lazy_loading: rooms::lazy_loading::Service {
                    db: db.clone(),
                    lazy_load_waiting: Mutex::new(HashMap::new()),
                },
                metadata: rooms::metadata::Service { db: db.clone() },
                outlier: rooms::outlier::Service { db: db.clone() },
                pdu_metadata: rooms::pdu_metadata::Service { db: db.clone() },
                search: rooms::search::Service { db: db.clone() },
                short: rooms::short::Service { db: db.clone() },
                state: rooms::state::Service { db: db.clone() },
                state_accessor: rooms::state_accessor::Service { db: db.clone() },
                state_cache: rooms::state_cache::Service { db: db.clone() },
                state_compressor: rooms::state_compressor::Service {
                    db: db.clone(),
                    stateinfo_cache: Mutex::new(LruCache::new(
                        (100.0 * config.conduit_cache_capacity_modifier) as usize,
                    )),
                },
                timeline: rooms::timeline::Service {
                    db: db.clone(),
                    lasttimelinecount_cache: Mutex::new(HashMap::new()),
                },
                user: rooms::user::Service { db: db.clone() },
            },
            transaction_ids: transaction_ids::Service { db: db.clone() },
            uiaa: uiaa::Service { db: db.clone() },
            users: users::Service { db: db.clone() },
            account_data: account_data::Service { db: db.clone() },
            admin: admin::Service { sender: todo!() },
            globals: globals::Service::load(db.clone(), config)?,
            key_backups: key_backups::Service { db: db.clone() },
            media: media::Service { db: db.clone() },
            sending: sending::Service {
                maximum_requests: todo!(),
                sender: todo!(),
            },
        })
    }
}
