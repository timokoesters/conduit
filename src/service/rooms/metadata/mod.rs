mod data;
pub use data::Data;
use ruma::RoomId;

use crate::Result;

pub struct Service {
    db: Box<dyn Data>,
}

impl Service {
    /// Checks if a room exists.
    #[tracing::instrument(skip(self))]
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        self.db.exists(room_id)
    }
}
