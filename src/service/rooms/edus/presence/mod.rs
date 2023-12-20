mod data;

use std::time::Duration;

pub use data::Data;
use futures_util::{stream::FuturesUnordered, StreamExt};
use ruma::{
    events::presence::{PresenceEvent, PresenceEventContent},
    presence::PresenceState,
    OwnedUserId, RoomId, UInt, UserId,
};
use serde::{Deserialize, Serialize};
use tokio::{sync::mpsc, time::sleep};
use tracing::debug;

use crate::{services, utils, Error, Result};

/// Represents data required to be kept in order to implement the presence specification.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Presence {
    pub state: PresenceState,
    pub currently_active: bool,
    pub last_active_ts: u64,
    pub last_count: u64,
    pub status_msg: Option<String>,
}

impl Presence {
    pub fn new(
        state: PresenceState,
        currently_active: bool,
        last_active_ts: u64,
        last_count: u64,
        status_msg: Option<String>,
    ) -> Self {
        Self {
            state,
            currently_active,
            last_active_ts,
            last_count,
            status_msg,
        }
    }

    pub fn from_json_bytes(bytes: &[u8]) -> Result<Self> {
        serde_json::from_slice(bytes)
            .map_err(|_| Error::bad_database("Invalid presence data in database"))
    }

    pub fn to_json_bytes(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(self)
            .map_err(|_| Error::bad_database("Could not serialize Presence to JSON"))
    }

    /// Creates a PresenceEvent from available data.
    pub fn to_presence_event(&self, user_id: &UserId) -> Result<PresenceEvent> {
        let now = utils::millis_since_unix_epoch();
        let last_active_ago = if self.currently_active {
            None
        } else {
            Some(UInt::new_saturating(
                now.saturating_sub(self.last_active_ts),
            ))
        };

        Ok(PresenceEvent {
            sender: user_id.to_owned(),
            content: PresenceEventContent {
                presence: self.state.clone(),
                status_msg: self.status_msg.clone(),
                currently_active: Some(self.currently_active),
                last_active_ago,
                displayname: services().users.displayname(user_id)?,
                avatar_url: services().users.avatar_url(user_id)?,
            },
        })
    }
}

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    /// Returns the latest presence event for the given user in the given room.
    pub fn get_presence(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<PresenceEvent>> {
        self.db.get_presence(room_id, user_id)
    }

    /// Pings the presence of the given user in the given room, setting the specified state.
    pub fn ping_presence(&self, user_id: &UserId, new_state: PresenceState) -> Result<()> {
        self.db.ping_presence(user_id, new_state)
    }

    /// Adds a presence event which will be saved until a new event replaces it.
    pub fn set_presence(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        presence_state: PresenceState,
        currently_active: Option<bool>,
        last_active_ago: Option<UInt>,
        status_msg: Option<String>,
    ) -> Result<()> {
        self.db.set_presence(
            room_id,
            user_id,
            presence_state,
            currently_active,
            last_active_ago,
            status_msg,
        )
    }

    /// Removes the presence record for the given user from the database.
    pub fn remove_presence(&self, user_id: &UserId) -> Result<()> {
        self.db.remove_presence(user_id)
    }

    /// Returns the most recent presence updates that happened after the event with id `since`.
    pub fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Box<dyn Iterator<Item = (OwnedUserId, u64, PresenceEvent)>> {
        self.db.presence_since(room_id, since)
    }
}

pub async fn presence_handler(
    mut presence_timer_receiver: mpsc::UnboundedReceiver<(OwnedUserId, Duration)>,
) -> Result<()> {
    let mut presence_timers = FuturesUnordered::new();

    loop {
        debug!("Number of presence timers: {}", presence_timers.len());

        tokio::select! {
            Some((user_id, timeout)) = presence_timer_receiver.recv() => {
                debug!("Adding timer for user '{user_id}': Timeout {timeout:?}");
                presence_timers.push(presence_timer(user_id, timeout));
            }

            Some(user_id) = presence_timers.next() => {
                process_presence_timer(user_id)?;
            }
        }
    }
}

async fn presence_timer(user_id: OwnedUserId, timeout: Duration) -> OwnedUserId {
    sleep(timeout).await;

    user_id
}

fn process_presence_timer(user_id: OwnedUserId) -> Result<()> {
    let idle_timeout = services().globals.config.presence_idle_timeout_s * 1_000;
    let offline_timeout = services().globals.config.presence_offline_timeout_s * 1_000;

    let mut presence_state = PresenceState::Offline;
    let mut last_active_ago = None;
    let mut status_msg = None;

    for room_id in services().rooms.state_cache.rooms_joined(&user_id) {
        let presence_event = services()
            .rooms
            .edus
            .presence
            .get_presence(&room_id?, &user_id)?;

        if let Some(presence_event) = presence_event {
            presence_state = presence_event.content.presence;
            last_active_ago = presence_event.content.last_active_ago;
            status_msg = presence_event.content.status_msg;

            break;
        }
    }

    let new_state = match (&presence_state, last_active_ago.map(u64::from)) {
        (PresenceState::Online, Some(ago)) if ago >= idle_timeout => {
            Some(PresenceState::Unavailable)
        }
        (PresenceState::Unavailable, Some(ago)) if ago >= offline_timeout => {
            Some(PresenceState::Offline)
        }
        _ => None,
    };

    debug!("Processed presence timer for user '{user_id}': Old state = {presence_state}, New state = {new_state:?}");

    if let Some(new_state) = new_state {
        for room_id in services().rooms.state_cache.rooms_joined(&user_id) {
            services().rooms.edus.presence.set_presence(
                &room_id?,
                &user_id,
                new_state.clone(),
                Some(false),
                last_active_ago,
                status_msg.clone(),
            )?;
        }
    }

    Ok(())
}
