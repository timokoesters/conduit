use ruma::{UserId, RoomId};
use crate::Result;

pub trait Data {
    fn reset_notification_counts(&self, user_id: &UserId, room_id: &RoomId) -> Result<()>;

    fn notification_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64>;

    fn highlight_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64>;

    fn associate_token_shortstatehash(
        &self,
        room_id: &RoomId,
        token: u64,
        shortstatehash: u64,
    ) -> Result<()>;

    fn get_token_shortstatehash(&self, room_id: &RoomId, token: u64) -> Result<Option<u64>>;

    fn get_shared_rooms<'a>(
        &'a self,
        users: Vec<Box<UserId>>,
    ) -> Result<Box<dyn Iterator<Item = Result<Box<RoomId>>>>>;
}
