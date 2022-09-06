use std::sync::Arc;

use ruma::{EventId, RoomId};

pub trait Data {
    fn mark_as_referenced(&self, room_id: &RoomId, event_ids: &[Arc<EventId>]) -> Result<()>;
    fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> Result<bool>;
    fn mark_event_soft_failed(&self, event_id: &EventId) -> Result<()>;
    fn is_event_soft_failed(&self, event_id: &EventId) -> Result<bool>;
}
