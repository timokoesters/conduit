use std::collections::HashMap;

use ruma::{UserId, RoomId, events::{RoomAccountDataEventType, AnyEphemeralRoomEvent}, serde::Raw};
use serde::{Serialize, de::DeserializeOwned};
use crate::Result;

pub trait Data {
    /// Places one event in the account data of the user and removes the previous entry.
    fn update<T: Serialize>(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        event_type: RoomAccountDataEventType,
        data: &T,
    ) -> Result<()>;

    /// Searches the account data for a specific kind.
    fn get<T: DeserializeOwned>(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        kind: RoomAccountDataEventType,
    ) -> Result<Option<T>>;

    /// Returns all changes to the account data that happened after `since`.
    fn changes_since(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        since: u64,
    ) -> Result<HashMap<RoomAccountDataEventType, Raw<AnyEphemeralRoomEvent>>>;
}
