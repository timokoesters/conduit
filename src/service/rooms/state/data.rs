use std::sync::Arc;
use std::{sync::MutexGuard, collections::HashSet};
use std::fmt::Debug;

use ruma::{EventId, RoomId};

pub trait Data {
    /// Returns the last state hash key added to the db for the given room.
    fn get_room_shortstatehash(room_id: &RoomId);

    /// Update the current state of the room.
    fn set_room_state(room_id: &RoomId, new_shortstatehash: u64,
        _mutex_lock: &MutexGuard<'_, StateLock>, // Take mutex guard to make sure users get the room state mutex
    );

    /// Associates a state with an event.
    fn set_event_state(shorteventid: u64, shortstatehash: u64) -> Result<()>;

    /// Returns all events we would send as the prev_events of the next event.
    fn get_forward_extremities(room_id: &RoomId) -> Result<HashSet<Arc<EventId>>>;

    /// Replace the forward extremities of the room.
    fn set_forward_extremities(
        room_id: &RoomId,
        event_ids: impl IntoIterator<Item = &'_ EventId> + Debug,
        _mutex_lock: &MutexGuard<'_, StateLock>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<()>;
}

pub struct StateLock;
