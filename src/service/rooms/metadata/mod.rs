mod data;
use std::sync::Arc;

pub use data::Data;
use ruma::RoomId;

use crate::Result;

pub struct Service {
    db: Arc<dyn Data>,
}

impl Service {
    /// Checks if a room exists.
    #[tracing::instrument(skip(self))]
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        self.db.exists(room_id)
    }

    pub fn is_disabled(&self, room_id: &RoomId) -> Result<bool> {
        self.db.is_disabled(room_id)
    }

    pub fn disable_room(&self, room_id: &RoomId, disabled: bool) -> Result<()> {
        self.db.disable_room(room_id, disabled)
    }
}
