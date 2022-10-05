use std::collections::HashMap;

use ruma::{UserId, RoomId, events::presence::PresenceEvent};
use crate::Result;

pub trait Data: Send + Sync {
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
    fn get_presence_event(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        count: u64,
    ) -> Result<Option<PresenceEvent>>;

    /// Returns the most recent presence updates that happened after the event with id `since`.
    fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<HashMap<Box<UserId>, PresenceEvent>>;
}
