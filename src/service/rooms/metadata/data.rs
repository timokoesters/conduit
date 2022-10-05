use ruma::RoomId;
use crate::Result;

pub trait Data: Send + Sync {
    fn exists(&self, room_id: &RoomId) -> Result<bool>;
    fn is_disabled(&self, room_id: &RoomId) -> Result<bool>;
    fn disable_room(&self, room_id: &RoomId, disabled: bool) -> Result<()>;
}
