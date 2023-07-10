use crate::Result;
use ruma::{
    events::presence::PresenceEvent, presence::PresenceState, OwnedUserId, RoomId, UInt, UserId,
};

pub trait Data: Send + Sync {
    /// Returns the latest presence event for the given user in the given room.
    fn get_presence(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<PresenceEvent>>;

    /// Pings the presence of the given user in the given room, setting the specified state.
    fn ping_presence(&self, user_id: &UserId, new_state: PresenceState) -> Result<()>;

    /// Adds a presence event which will be saved until a new event replaces it.
    fn set_presence(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        presence_state: PresenceState,
        currently_active: Option<bool>,
        last_active_ago: Option<UInt>,
        status_msg: Option<String>,
    ) -> Result<()>;

    /// Removes the presence record for the given user from the database.
    fn remove_presence(&self, user_id: &UserId) -> Result<()>;

    /// Returns the most recent presence updates that happened after the event with id `since`.
    fn presence_since<'a>(
        &'a self,
        room_id: &RoomId,
        since: u64,
    ) -> Box<dyn Iterator<Item = Result<(OwnedUserId, u64, PresenceEvent)>> + 'a>;
}
