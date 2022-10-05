use std::collections::HashMap;

use crate::Result;
use ruma::{
    events::{AnyEphemeralRoomEvent, RoomAccountDataEventType},
    serde::Raw,
    RoomId, UserId,
};

pub trait Data: Send + Sync {
    /// Places one event in the account data of the user and removes the previous entry.
    fn update(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        event_type: RoomAccountDataEventType,
        data: &serde_json::Value,
    ) -> Result<()>;

    /// Searches the account data for a specific kind.
    fn get(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        kind: RoomAccountDataEventType,
    ) -> Result<Option<Box<serde_json::value::RawValue>>>;

    /// Returns all changes to the account data that happened after `since`.
    fn changes_since(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        since: u64,
    ) -> Result<HashMap<RoomAccountDataEventType, Raw<AnyEphemeralRoomEvent>>>;
}
