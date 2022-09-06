use ruma::{UserId, RoomId};

pub trait Data {
    fn mark_as_once_joined(user_id: &UserId, room_id: &RoomId) -> Result<()>;
}
