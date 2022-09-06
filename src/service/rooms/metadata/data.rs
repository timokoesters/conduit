use ruma::RoomId;

pub trait Data {
    fn exists(&self, room_id: &RoomId) -> Result<bool>;
}
