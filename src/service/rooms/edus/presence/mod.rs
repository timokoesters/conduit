mod data;

pub use data::Data;
use ruma::{events::presence::PresenceEvent, OwnedUserId, RoomId, UserId};
use tokio::sync::mpsc;

use crate::{Error, Result};

pub struct Service {
    pub db: &'static dyn Data,

    // Presence timers
    timer_sender: mpsc::UnboundedSender<OwnedUserId>,
}

impl Service {
    /// Builds the service and initialized the presence_maintain task
    pub fn build(db: &'static dyn Data) -> Result<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        let service = Self {
            db,
            timer_sender: sender,
        };

        service.presence_maintain(receiver)?;
        service.presence_cleanup()?;
        
        Ok(service)
    }

    /// Resets the presence timeout, so the user will stay in their current presence state.
    pub fn ping_presence(
        &self,
        user_id: &UserId,
        update_count: bool,
        update_timestamp: bool,
        spawn_timer: bool,
    ) -> Result<()> {
        if spawn_timer {
            self.spawn_timer(user_id)?;
        }

        self.db
            .ping_presence(user_id, update_count, update_timestamp)
    }

    /// Adds a presence event which will be saved until a new event replaces it.
    ///
    /// Note: This method takes a RoomId because presence updates are always bound to rooms to
    /// make sure users outside these rooms can't see them.
    pub fn update_presence(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        presence: PresenceEvent,
        spawn_timer: bool,
    ) -> Result<()> {
        if spawn_timer {
            self.spawn_timer(user_id)?;
        }

        self.db.update_presence(user_id, room_id, presence)
    }

    /// Returns the timestamp of when the presence was last updated for the specified user.
    pub fn last_presence_update(&self, user_id: &UserId) -> Result<Option<(u64, u64)>> {
        self.db.last_presence_update(user_id)
    }

    /// Returns the saved presence event for this user with actual last_active_ago.
    pub fn get_presence_event(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<PresenceEvent>> {
        let last_update = match self.db.last_presence_update(user_id)? {
            Some(last) => last.1,
            None => return Ok(None),
        };

        self.db.get_presence_event(room_id, user_id, last_update)
    }

    /// Returns the most recent presence updates that happened after the event with id `since`.
    #[tracing::instrument(skip(self, since, room_id))]
    pub fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<Box<dyn Iterator<Item = (OwnedUserId, PresenceEvent)>>> {
        self.db.presence_since(room_id, since)
    }

    /// Spawns a task maintaining presence data
    fn presence_maintain(
        &self,
        timer_receiver: mpsc::UnboundedReceiver<OwnedUserId>,
    ) -> Result<()> {
        self.db.presence_maintain(timer_receiver)
    }

    fn presence_cleanup(&self) -> Result<()> {
        self.db.presence_cleanup()
    }

    /// Spawns a timer for the user used by the maintenance task
    fn spawn_timer(&self, user_id: &UserId) -> Result<()> {
        self.timer_sender
            .send(user_id.into())
            .map_err(|_| Error::bad_database("Sender errored out"))?;

        Ok(())
    }
}
