pub trait Data {
    /// Replaces the previous read receipt.
    fn readreceipt_update(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        event: ReceiptEvent,
    ) -> Result<()>;

    /// Returns an iterator over the most recent read_receipts in a room that happened after the event with id `since`.
    fn readreceipts_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> impl Iterator<
        Item = Result<(
            Box<UserId>,
            u64,
            Raw<ruma::events::AnySyncEphemeralRoomEvent>,
        )>,
    >;

    /// Sets a private read marker at `count`.
    fn private_read_set(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        count: u64,
    ) -> Result<()>;

    /// Returns the private read marker.
    fn private_read_get(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>>;

    /// Returns the count of the last typing update in this room.
    fn last_privateread_update(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64>;

    /// Sets a user as typing until the timeout timestamp is reached or roomtyping_remove is
    /// called.
    fn typing_add(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        timeout: u64,
    ) -> Result<()>;

    /// Removes a user from typing before the timeout is reached.
    fn typing_remove(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<()>;

    /// Returns the count of the last typing update in this room.
    fn last_typing_update(
        &self,
        room_id: &RoomId,
    ) -> Result<u64>;

    /// Returns all user ids currently typing.
    fn typings_all(
        &self,
        room_id: &RoomId,
    ) -> Result<HashSet<UserId>>;

    /// Adds a presence event which will be saved until a new event replaces it.
    ///
    /// Note: This method takes a RoomId because presence updates are always bound to rooms to
    /// make sure users outside these rooms can't see them.
    fn update_presence(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        presence: PresenceEvent,
    ) -> Result<()>;

    /// Resets the presence timeout, so the user will stay in their current presence state.
    fn ping_presence(&self, user_id: &UserId) -> Result<()>;

    /// Returns the timestamp of the last presence update of this user in millis since the unix epoch.
    fn last_presence_update(&self, user_id: &UserId) -> Result<Option<u64>>;

    /// Returns the presence event with correct last_active_ago.
    fn get_presence_event(&self, room_id: &RoomId, user_id: &UserId, count: u64) -> Result<Option<PresenceEvent>>;

    /// Returns the most recent presence updates that happened after the event with id `since`.
    fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<HashMap<Box<UserId>, PresenceEvent>>;
}
