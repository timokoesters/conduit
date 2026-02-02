use std::sync::Arc;

use crate::{service::rooms::timeline::PduCount, PduEvent, Result};
use ruma::{EventId, RoomId, UserId};

pub trait Data: Send + Sync {
    fn add_relation(&self, from: u64, to: u64) -> Result<()>;
    #[allow(clippy::type_complexity)]
    fn relations_until<'a>(
        &'a self,
        user_id: &'a UserId,
        room_id: u64,
        target: u64,
        until: PduCount,
    ) -> Result<Box<dyn Iterator<Item = Result<(PduCount, PduEvent)>> + 'a>>;
    fn mark_as_referenced(&self, room_id: &RoomId, event_ids: &[Arc<EventId>]) -> Result<()>;
    fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> Result<bool>;
    fn mark_event_soft_failed(&self, event_id: &EventId) -> Result<()>;
    fn is_event_soft_failed(&self, event_id: &EventId) -> Result<bool>;
}
