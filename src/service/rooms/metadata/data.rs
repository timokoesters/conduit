use ruma::RoomId;
use crate::Result;

pub trait Data: Send + Sync {
    fn exists(&self, room_id: &RoomId) -> Result<bool>;
}
