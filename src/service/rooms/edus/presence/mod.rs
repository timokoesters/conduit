mod data;
use std::{collections::HashMap, sync::Arc};

pub use data::Data;
use ruma::{RoomId, UserId, events::presence::PresenceEvent};

use crate::Result;

pub struct Service {
    db: Arc<dyn Data>,
}

impl Service {
    /// Adds a presence event which will be saved until a new event replaces it.
    ///
    /// Note: This method takes a RoomId because presence updates are always bound to rooms to
    /// make sure users outside these rooms can't see them.
    pub fn update_presence(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        presence: PresenceEvent,
    ) -> Result<()> {
        self.db.update_presence(user_id, room_id, presence)
    }

    /// Resets the presence timeout, so the user will stay in their current presence state.
    pub fn ping_presence(&self, user_id: &UserId) -> Result<()> {
        self.db.ping_presence(user_id)
    }

    pub fn get_last_presence_event(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<PresenceEvent>> {
        let last_update = match self.db.last_presence_update(user_id)? {
            Some(last) => last,
            None => return Ok(None),
        };

        self.db.get_presence_event(room_id, user_id, last_update)
    }

    /* TODO
    /// Sets all users to offline who have been quiet for too long.
    fn _presence_maintain(
        &self,
        rooms: &super::Rooms,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let current_timestamp = utils::millis_since_unix_epoch();

        for (user_id_bytes, last_timestamp) in self
            .userid_lastpresenceupdate
            .iter()
            .filter_map(|(k, bytes)| {
                Some((
                    k,
                    utils::u64_from_bytes(&bytes)
                        .map_err(|_| {
                            Error::bad_database("Invalid timestamp in userid_lastpresenceupdate.")
                        })
                        .ok()?,
                ))
            })
            .take_while(|(_, timestamp)| current_timestamp.saturating_sub(*timestamp) > 5 * 60_000)
        // 5 Minutes
        {
            // Send new presence events to set the user offline
            let count = globals.next_count()?.to_be_bytes();
            let user_id: Box<_> = utils::string_from_bytes(&user_id_bytes)
                .map_err(|_| {
                    Error::bad_database("Invalid UserId bytes in userid_lastpresenceupdate.")
                })?
                .try_into()
                .map_err(|_| Error::bad_database("Invalid UserId in userid_lastpresenceupdate."))?;
            for room_id in rooms.rooms_joined(&user_id).filter_map(|r| r.ok()) {
                let mut presence_id = room_id.as_bytes().to_vec();
                presence_id.push(0xff);
                presence_id.extend_from_slice(&count);
                presence_id.push(0xff);
                presence_id.extend_from_slice(&user_id_bytes);

                self.presenceid_presence.insert(
                    &presence_id,
                    &serde_json::to_vec(&PresenceEvent {
                        content: PresenceEventContent {
                            avatar_url: None,
                            currently_active: None,
                            displayname: None,
                            last_active_ago: Some(
                                last_timestamp.try_into().expect("time is valid"),
                            ),
                            presence: PresenceState::Offline,
                            status_msg: None,
                        },
                        sender: user_id.to_owned(),
                    })
                    .expect("PresenceEvent can be serialized"),
                )?;
            }

            self.userid_lastpresenceupdate.insert(
                user_id.as_bytes(),
                &utils::millis_since_unix_epoch().to_be_bytes(),
            )?;
        }

        Ok(())
    }*/

    /// Returns the most recent presence updates that happened after the event with id `since`.
    #[tracing::instrument(skip(self, since, room_id))]
    pub fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<HashMap<Box<UserId>, PresenceEvent>> {
        self.db.presence_since(room_id, since)
    }
}
