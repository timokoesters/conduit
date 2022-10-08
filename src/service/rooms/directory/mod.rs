mod data;

pub use data::Data;
use ruma::RoomId;

use crate::Result;

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    #[tracing::instrument(skip(self))]
    pub fn set_public(&self, room_id: &RoomId) -> Result<()> {
        self.db.set_public(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn set_not_public(&self, room_id: &RoomId) -> Result<()> {
        self.db.set_not_public(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_public_room(&self, room_id: &RoomId) -> Result<bool> {
        self.db.is_public_room(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn public_rooms(&self) -> impl Iterator<Item = Result<Box<RoomId>>> + '_ {
        self.db.public_rooms()
    }
}
