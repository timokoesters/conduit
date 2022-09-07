mod data;
pub use data::Data;
use ruma::{RoomId, UserId};

use crate::Result;

pub struct Service<D: Data> {
    db: D,
}

impl<D: Data> Service<D> {
    pub fn reset_notification_counts(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        self.db.reset_notification_counts(user_id, room_id)
    }

    pub fn notification_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        self.db.notification_count(user_id, room_id)
    }

    pub fn highlight_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        self.db.highlight_count(user_id, room_id)
    }

    pub fn associate_token_shortstatehash(
        &self,
        room_id: &RoomId,
        token: u64,
        shortstatehash: u64,
    ) -> Result<()> {
        self.db.associate_token_shortstatehash(room_id, token, shortstatehash)
    }

    pub fn get_token_shortstatehash(&self, room_id: &RoomId, token: u64) -> Result<Option<u64>> {
        self.db.get_token_shortstatehash(room_id, token)
    }

    pub fn get_shared_rooms<'a>(
        &'a self,
        users: Vec<Box<UserId>>,
    ) -> Result<impl Iterator<Item = Result<Box<RoomId>>> + 'a> {
        self.db.get_shared_rooms(users)
    }
}
