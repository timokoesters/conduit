use std::sync::Arc;

use ruma::{RoomId, EventId};

use crate::{service, database::KeyValueDatabase};

impl service::rooms::pdu_metadata::Data for KeyValueDatabase {
    fn mark_as_referenced(&self, room_id: &RoomId, event_ids: &[Arc<EventId>]) -> Result<()> {
        for prev in event_ids {
            let mut key = room_id.as_bytes().to_vec();
            key.extend_from_slice(prev.as_bytes());
            self.referencedevents.insert(&key, &[])?;
        }

        Ok(())
    }

    fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> Result<bool> {
        let mut key = room_id.as_bytes().to_vec();
        key.extend_from_slice(event_id.as_bytes());
        Ok(self.referencedevents.get(&key)?.is_some())
    }

    fn mark_event_soft_failed(&self, event_id: &EventId) -> Result<()> {
        self.softfailedeventids.insert(event_id.as_bytes(), &[])
    }

    fn is_event_soft_failed(&self, event_id: &EventId) -> Result<bool> {
        self.softfailedeventids
            .get(event_id.as_bytes())
            .map(|o| o.is_some())
    }
}
