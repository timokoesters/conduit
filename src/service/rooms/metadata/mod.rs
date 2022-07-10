mod data;
pub use data::Data;

use crate::service::*;

pub struct Service<D: Data> {
    db: D,
}

impl Service<_> {
    /// Checks if a room exists.
    #[tracing::instrument(skip(self))]
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        self.db.exists(room_id)
    }
}
