use std::{
    collections::HashMap,
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
    pub admin: Arc<admin::Service>,
    pub globals: globals::Service,
    pub key_backups: key_backups::Service,
    pub media: media::Service,
    pub sending: Arc<sending::Service>,
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
            + media::Data
            + sending::Data
            + 'static,
    >(
        db: &'static D,
        config: Config,
    ) -> Result<Self> {
        Ok(Self {
            appservice: appservice::Service { db },
            pusher: pusher::Service { db },
            rooms: rooms::Service {
                alias: rooms::alias::Service { db },
                auth_chain: rooms::auth_chain::Service { db },
                directory: rooms::directory::Service { db },
                edus: rooms::edus::Service {
                    presence: rooms::edus::presence::Service::build(db)?,
                    read_receipt: rooms::edus::read_receipt::Service { db },
                    typing: rooms::edus::typing::Service { db },
                },
                event_handler: rooms::event_handler::Service,
                lazy_loading: rooms::lazy_loading::Service {
                    db,
                    lazy_load_waiting: Mutex::new(HashMap::new()),
                },
                metadata: rooms::metadata::Service { db },
                outlier: rooms::outlier::Service { db },
                pdu_metadata: rooms::pdu_metadata::Service { db },
                search: rooms::search::Service { db },
                short: rooms::short::Service { db },
                state: rooms::state::Service { db },
                state_accessor: rooms::state_accessor::Service { db },
                state_cache: rooms::state_cache::Service { db },
                state_compressor: rooms::state_compressor::Service {
                    db,
                    stateinfo_cache: Mutex::new(LruCache::new(
                        (100.0 * config.conduit_cache_capacity_modifier) as usize,
                    )),
                },
                timeline: rooms::timeline::Service {
                    db,
                    lasttimelinecount_cache: Mutex::new(HashMap::new()),
                },
                user: rooms::user::Service { db },
            },
            transaction_ids: transaction_ids::Service { db },
            uiaa: uiaa::Service { db },
            users: users::Service { db },
            account_data: account_data::Service { db },
            admin: admin::Service::build(),
            key_backups: key_backups::Service { db },
            media: media::Service { db },
            sending: sending::Service::build(db, &config),

            globals: globals::Service::load(db, config)?,
        })
    }
}
