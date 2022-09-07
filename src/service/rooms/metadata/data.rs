use ruma::RoomId;
use crate::Result;

pub trait Data {
    fn exists(&self, room_id: &RoomId) -> Result<bool>;
}
