pub mod pdu;
pub mod appservice;
pub mod pusher;
pub mod rooms;
pub mod transaction_ids;
pub mod uiaa;
pub mod users;
pub mod account_data;
pub mod admin;
pub mod globals;
pub mod key_backups;
pub mod media;
pub mod sending;

pub struct Services<D> {
    pub appservice: appservice::Service<D>,
    pub pusher: pusher::Service<D>,
    pub rooms: rooms::Service<D>,
    pub transaction_ids: transaction_ids::Service<D>,
    pub uiaa: uiaa::Service<D>,
    pub users: users::Service<D>,
    //pub account_data: account_data::Service<D>,
    //pub admin: admin::Service<D>,
    pub globals: globals::Service<D>,
    //pub key_backups: key_backups::Service<D>,
    //pub media: media::Service<D>,
    //pub sending: sending::Service<D>,
}
