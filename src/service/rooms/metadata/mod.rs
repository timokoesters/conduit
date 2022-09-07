mod data;
pub use data::Data;
use ruma::RoomId;

use crate::Result;

pub struct Service<D: Data> {
    db: D,
}

impl<D: Data> Service<D> {
    /// Checks if a room exists.
    #[tracing::instrument(skip(self))]
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        self.db.exists(room_id)
    }
}
