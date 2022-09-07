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

pub struct Services<D: appservice::Data + pusher::Data + rooms::Data + transaction_ids::Data + uiaa::Data + users::Data + account_data::Data + globals::Data + key_backups::Data + media::Data>
{
    pub appservice: appservice::Service<D>,
    pub pusher: pusher::Service<D>,
    pub rooms: rooms::Service<D>,
    pub transaction_ids: transaction_ids::Service<D>,
    pub uiaa: uiaa::Service<D>,
    pub users: users::Service<D>,
    pub account_data: account_data::Service<D>,
    pub admin: admin::Service,
    pub globals: globals::Service<D>,
    pub key_backups: key_backups::Service<D>,
    pub media: media::Service<D>,
    pub sending: sending::Service,
}
