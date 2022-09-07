mod data;
pub use data::Data;
use ruma::{UserId, RoomId, events::SyncEphemeralRoomEvent};

use crate::Result;

pub struct Service<D: Data> {
    db: D,
}

impl<D: Data> Service<D> {
    /// Sets a user as typing until the timeout timestamp is reached or roomtyping_remove is
    /// called.
    pub fn typing_add(&self, user_id: &UserId, room_id: &RoomId, timeout: u64) -> Result<()> {
        self.db.typing_add(user_id, room_id, timeout)
    }

    /// Removes a user from typing before the timeout is reached.
    pub fn typing_remove(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        self.db.typing_remove(user_id, room_id)
    }

    /* TODO: Do this in background thread?
    /// Makes sure that typing events with old timestamps get removed.
    fn typings_maintain(
        &self,
        room_id: &RoomId,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let current_timestamp = utils::millis_since_unix_epoch();

        let mut found_outdated = false;

        // Find all outdated edus before inserting a new one
        for outdated_edu in self
            .typingid_userid
            .scan_prefix(prefix)
            .map(|(key, _)| {
                Ok::<_, Error>((
                    key.clone(),
                    utils::u64_from_bytes(
                        &key.splitn(2, |&b| b == 0xff).nth(1).ok_or_else(|| {
                            Error::bad_database("RoomTyping has invalid timestamp or delimiters.")
                        })?[0..mem::size_of::<u64>()],
                    )
                    .map_err(|_| Error::bad_database("RoomTyping has invalid timestamp bytes."))?,
                ))
            })
            .filter_map(|r| r.ok())
            .take_while(|&(_, timestamp)| timestamp < current_timestamp)
        {
            // This is an outdated edu (time > timestamp)
            self.typingid_userid.remove(&outdated_edu.0)?;
            found_outdated = true;
        }

        if found_outdated {
            self.roomid_lasttypingupdate
                .insert(room_id.as_bytes(), &globals.next_count()?.to_be_bytes())?;
        }

        Ok(())
    }
    */

    /// Returns the count of the last typing update in this room.
    pub fn last_typing_update(&self, room_id: &RoomId) -> Result<u64> {
        self.db.last_typing_update(room_id)
    }

    /// Returns a new typing EDU.
    pub fn typings_all(
        &self,
        room_id: &RoomId,
    ) -> Result<SyncEphemeralRoomEvent<ruma::events::typing::TypingEventContent>> {
        let user_ids = self.db.typings_all(room_id)?;

        Ok(SyncEphemeralRoomEvent {
            content: ruma::events::typing::TypingEventContent {
                user_ids: user_ids.into_iter().collect(),
            },
        })
    }
}
