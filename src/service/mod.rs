use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex as StdMutex},
};

use lru_cache::LruCache;
use tokio::sync::{broadcast, Mutex};

use crate::{Config, Result};
use tokio::sync::RwLock;

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
    // Results in large capacity if set to a negative number, user's fault really :P
    #[allow(clippy::cast_sign_loss)]
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
            appservice: appservice::Service::build(db)?,
            pusher: pusher::Service { db },
            rooms: rooms::Service {
                alias: rooms::alias::Service { db },
                auth_chain: rooms::auth_chain::Service { db },
                directory: rooms::directory::Service { db },
                edus: rooms::edus::Service {
                    presence: rooms::edus::presence::Service { db },
                    read_receipt: rooms::edus::read_receipt::Service { db },
                    typing: rooms::edus::typing::Service {
                        typing: RwLock::new(BTreeMap::new()),
                        last_typing_update: RwLock::new(BTreeMap::new()),
                        typing_update_sender: broadcast::channel(100).0,
                    },
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
                state_accessor: rooms::state_accessor::Service {
                    db,
                    server_visibility_cache: StdMutex::new(LruCache::new(
                        (100.0 * config.conduit_cache_capacity_modifier) as usize,
                    )),
                    user_visibility_cache: StdMutex::new(LruCache::new(
                        (100.0 * config.conduit_cache_capacity_modifier) as usize,
                    )),
                },
                state_cache: rooms::state_cache::Service { db },
                state_compressor: rooms::state_compressor::Service {
                    db,
                    stateinfo_cache: StdMutex::new(LruCache::new(
                        (100.0 * config.conduit_cache_capacity_modifier) as usize,
                    )),
                },
                timeline: rooms::timeline::Service {
                    db,
                    lasttimelinecount_cache: Mutex::new(HashMap::new()),
                },
                threads: rooms::threads::Service { db },
                spaces: rooms::spaces::Service {
                    roomid_spacechunk_cache: Mutex::new(LruCache::new(200)),
                },
                user: rooms::user::Service { db },
            },
            transaction_ids: transaction_ids::Service { db },
            uiaa: uiaa::Service { db },
            users: users::Service {
                db,
                connections: StdMutex::new(BTreeMap::new()),
            },
            account_data: account_data::Service { db },
            admin: admin::Service::build(),
            key_backups: key_backups::Service { db },
            media: media::Service { db },
            sending: sending::Service::build(db, &config),

            globals: globals::Service::load(db, config)?,
        })
    }
    async fn memory_usage(&self) -> String {
        let lazy_load_waiting = self.rooms.lazy_loading.lazy_load_waiting.lock().await.len();
        let server_visibility_cache = self
            .rooms
            .state_accessor
            .server_visibility_cache
            .lock()
            .unwrap()
            .len();
        let user_visibility_cache = self
            .rooms
            .state_accessor
            .user_visibility_cache
            .lock()
            .unwrap()
            .len();
        let stateinfo_cache = self
            .rooms
            .state_compressor
            .stateinfo_cache
            .lock()
            .unwrap()
            .len();
        let lasttimelinecount_cache = self
            .rooms
            .timeline
            .lasttimelinecount_cache
            .lock()
            .await
            .len();
        let roomid_spacechunk_cache = self.rooms.spaces.roomid_spacechunk_cache.lock().await.len();

        format!(
            "\
lazy_load_waiting: {lazy_load_waiting}
server_visibility_cache: {server_visibility_cache}
user_visibility_cache: {user_visibility_cache}
stateinfo_cache: {stateinfo_cache}
lasttimelinecount_cache: {lasttimelinecount_cache}
roomid_spacechunk_cache: {roomid_spacechunk_cache}\
            "
        )
    }
    async fn clear_caches(&self, amount: u32) {
        if amount > 0 {
            self.rooms
                .lazy_loading
                .lazy_load_waiting
                .lock()
                .await
                .clear();
        }
        if amount > 1 {
            self.rooms
                .state_accessor
                .server_visibility_cache
                .lock()
                .unwrap()
                .clear();
        }
        if amount > 2 {
            self.rooms
                .state_accessor
                .user_visibility_cache
                .lock()
                .unwrap()
                .clear();
        }
        if amount > 3 {
            self.rooms
                .state_compressor
                .stateinfo_cache
                .lock()
                .unwrap()
                .clear();
        }
        if amount > 4 {
            self.rooms
                .timeline
                .lasttimelinecount_cache
                .lock()
                .await
                .clear();
        }
        if amount > 5 {
            self.rooms
                .spaces
                .roomid_spacechunk_cache
                .lock()
                .await
                .clear();
        }
    }
}
