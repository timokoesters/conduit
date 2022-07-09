mod data;
pub use data::Data;

use crate::service::*;

pub struct Service<D: Data> {
    db: D,
}

impl Service<_> {
    #[tracing::instrument(skip(self))]
    pub fn set_public(&self, room_id: &RoomId) -> Result<()> {
        self.db.set_public(&self, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn set_not_public(&self, room_id: &RoomId) -> Result<()> {
        self.db.set_not_public(&self, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_public_room(&self, room_id: &RoomId) -> Result<bool> {
        self.db.is_public_room(&self, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn public_rooms(&self) -> impl Iterator<Item = Result<Box<RoomId>>> + '_ {
        self.db.public_rooms(&self, room_id)
    }
}
