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

pub struct Service<D: Data> {
    pub alias: alias::Service<D>,
    pub auth_chain: auth_chain::Service<D>,
    pub directory: directory::Service<D>,
    pub edus: edus::Service<D>,
    pub event_handler: event_handler::Service,
    pub lazy_loading: lazy_loading::Service<D>,
    pub metadata: metadata::Service<D>,
    pub outlier: outlier::Service<D>,
    pub pdu_metadata: pdu_metadata::Service<D>,
    pub search: search::Service<D>,
    pub short: short::Service<D>,
    pub state: state::Service<D>,
    pub state_accessor: state_accessor::Service<D>,
    pub state_cache: state_cache::Service<D>,
    pub state_compressor: state_compressor::Service<D>,
    pub timeline: timeline::Service<D>,
    pub user: user::Service<D>,
}
