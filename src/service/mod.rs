use std::sync::Arc;

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
    pub fn build<D: appservice::Data + pusher::Data + rooms::Data + transaction_ids::Data + uiaa::Data + users::Data + account_data::Data + globals::Data + key_backups::Data + media::Data>(db: Arc<D>) -> Self {
        Self {
            appservice: appservice::Service { db: db.clone() },
            pusher: pusher::Service { db: db.clone() },
            rooms: rooms::Service { db: Arc::clone(&db) },
            transaction_ids: transaction_ids::Service { db: Arc::clone(&db) },
            uiaa: uiaa::Service { db: Arc::clone(&db) },
            users: users::Service { db: Arc::clone(&db) },
            account_data: account_data::Service { db: Arc::clone(&db) },
            admin: admin::Service { db: Arc::clone(&db) },
            globals: globals::Service { db: Arc::clone(&db) },
            key_backups: key_backups::Service { db: Arc::clone(&db) },
            media: media::Service { db: Arc::clone(&db) },
            sending: sending::Service { db: Arc::clone(&db) },
        }
    }
}
