use ruma::{UserId, RoomId, serde::Raw, events::AnyStrippedStateEvent};
use crate::Result;

pub trait Data {
    fn mark_as_once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<()>;
    fn mark_as_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<()>;
    fn mark_as_invited(&self, user_id: &UserId, room_id: &RoomId, last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>) -> Result<()>;
    fn mark_as_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<()>;
}
