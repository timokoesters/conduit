use std::sync::Arc;

use ruma::{EventId, events::StateEventType, RoomId};
use crate::Result;

pub trait Data: Send + Sync {
    fn get_or_create_shorteventid(
        &self,
        event_id: &EventId,
    ) -> Result<u64>;

    fn get_shortstatekey(
        &self,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<u64>>;

    fn get_or_create_shortstatekey(
        &self,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<u64>;

    fn get_eventid_from_short(&self, shorteventid: u64) -> Result<Arc<EventId>>;

    fn get_statekey_from_short(&self, shortstatekey: u64) -> Result<(StateEventType, String)>;

    /// Returns (shortstatehash, already_existed)
    fn get_or_create_shortstatehash(
        &self,
        state_hash: &[u8],
    ) -> Result<(u64, bool)>;

    fn get_shortroomid(&self, room_id: &RoomId) -> Result<Option<u64>>;

    fn get_or_create_shortroomid(
        &self,
        room_id: &RoomId,
    ) -> Result<u64>;
}
