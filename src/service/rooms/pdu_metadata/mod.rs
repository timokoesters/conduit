mod data;
use std::sync::Arc;

pub use data::Data;
use ruma::{RoomId, EventId};

use crate::Result;

pub struct Service {
    db: Arc<dyn Data>,
}

impl Service {
    #[tracing::instrument(skip(self, room_id, event_ids))]
    pub fn mark_as_referenced(&self, room_id: &RoomId, event_ids: &[Arc<EventId>]) -> Result<()> {
        self.db.mark_as_referenced(room_id, event_ids)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> Result<bool> {
        self.db.is_event_referenced(room_id, event_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn mark_event_soft_failed(&self, event_id: &EventId) -> Result<()> {
        self.db.mark_event_soft_failed(event_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_event_soft_failed(&self, event_id: &EventId) -> Result<bool> {
        self.db.is_event_soft_failed(event_id)
    }
}
