pub mod alias;
pub mod auth_chain;
pub mod directory;
pub mod edus;
pub mod event_handler;
pub mod lazy_loading;
pub mod metadata;
pub mod outlier;
pub mod pdu_metadata;
pub mod search;
pub mod short;
pub mod state;
pub mod state_accessor;
pub mod state_cache;
pub mod state_compressor;
pub mod timeline;
pub mod user;

pub trait Data: alias::Data + auth_chain::Data + directory::Data + edus::Data + lazy_loading::Data + metadata::Data + outlier::Data + pdu_metadata::Data + search::Data + short::Data + state::Data + state_accessor::Data + state_cache::Data + state_compressor::Data + timeline::Data + user::Data {}

pub struct Service {
    pub alias: alias::Service,
    pub auth_chain: auth_chain::Service,
    pub directory: directory::Service,
    pub edus: edus::Service,
    pub event_handler: event_handler::Service,
    pub lazy_loading: lazy_loading::Service,
    pub metadata: metadata::Service,
    pub outlier: outlier::Service,
    pub pdu_metadata: pdu_metadata::Service,
    pub search: search::Service,
    pub short: short::Service,
    pub state: state::Service,
    pub state_accessor: state_accessor::Service,
    pub state_cache: state_cache::Service,
    pub state_compressor: state_compressor::Service,
    pub timeline: timeline::Service,
    pub user: user::Service,
}
