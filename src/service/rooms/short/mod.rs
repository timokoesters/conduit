mod data;
use std::sync::Arc;

pub use data::Data;
use ruma::{events::StateEventType, EventId, RoomId};

use crate::{Result};

pub struct Service {
    db: Arc<dyn Data>,
}

impl Service {
    pub fn get_or_create_shorteventid(&self, event_id: &EventId) -> Result<u64> {
        self.db.get_or_create_shorteventid(event_id)
    }

    pub fn get_shortstatekey(
        &self,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<u64>> {
        self.db.get_shortstatekey(event_type, state_key)
    }

    pub fn get_or_create_shortstatekey(
        &self,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<u64> {
        self.db.get_or_create_shortstatekey(event_type, state_key)
    }

    pub fn get_eventid_from_short(&self, shorteventid: u64) -> Result<Arc<EventId>> {
        self.db.get_eventid_from_short(shorteventid)
    }

    pub fn get_statekey_from_short(&self, shortstatekey: u64) -> Result<(StateEventType, String)> {
        self.db.get_statekey_from_short(shortstatekey)
    }

    /// Returns (shortstatehash, already_existed)
    pub fn get_or_create_shortstatehash(&self, state_hash: &[u8]) -> Result<(u64, bool)> {
        self.db.get_or_create_shortstatehash(state_hash)
    }

    pub fn get_shortroomid(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.db.get_shortroomid(room_id)
    }

    pub fn get_or_create_shortroomid(&self, room_id: &RoomId) -> Result<u64> {
        self.db.get_or_create_shortroomid(room_id)
    }
}
