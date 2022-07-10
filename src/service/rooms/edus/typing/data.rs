pub trait Data {
    /// Sets a user as typing until the timeout timestamp is reached or roomtyping_remove is
    /// called.
    fn typing_add(&self, user_id: &UserId, room_id: &RoomId, timeout: u64) -> Result<()>;

    /// Removes a user from typing before the timeout is reached.
    fn typing_remove(&self, user_id: &UserId, room_id: &RoomId) -> Result<()>;

    /// Returns the count of the last typing update in this room.
    fn last_typing_update(&self, room_id: &RoomId) -> Result<u64>;

    /// Returns all user ids currently typing.
    fn typings_all(&self, room_id: &RoomId) -> Result<HashSet<UserId>>;
}
