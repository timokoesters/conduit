mod data;
pub use data::Data;

use crate::service::*;

pub struct Service<D: Data> {
    db: D,
}

impl Service<_> {
    /// Replaces the previous read receipt.
    pub fn readreceipt_update(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        event: ReceiptEvent,
    ) -> Result<()> {
        self.db.readreceipt_update(user_id, room_id, event);
    }

    /// Returns an iterator over the most recent read_receipts in a room that happened after the event with id `since`.
    #[tracing::instrument(skip(self))]
    pub fn readreceipts_since<'a>(
        &'a self,
        room_id: &RoomId,
        since: u64,
    ) -> impl Iterator<
        Item = Result<(
            Box<UserId>,
            u64,
            Raw<ruma::events::AnySyncEphemeralRoomEvent>,
        )>,
    > + 'a {
        self.db.readreceipts_since(room_id, since)
    }

    /// Sets a private read marker at `count`.
    #[tracing::instrument(skip(self, globals))]
    pub fn private_read_set(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        count: u64,
    ) -> Result<()> {
        self.db.private_read_set(room_id, user_id, count)
    }

    /// Returns the private read marker.
    #[tracing::instrument(skip(self))]
    pub fn private_read_get(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        self.db.private_read_get(room_id, user_id)
    }

    /// Returns the count of the last typing update in this room.
    pub fn last_privateread_update(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        self.db.last_privateread_update(user_id, room_id)
    }

    /// Sets a user as typing until the timeout timestamp is reached or roomtyping_remove is
    /// called.
    pub fn typing_add(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        timeout: u64,
    ) -> Result<()> {
        self.db.typing_add(user_id, room_id, timeout)
    }

    /// Removes a user from typing before the timeout is reached.
    pub fn typing_remove(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<()> {
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
    #[tracing::instrument(skip(self, globals))]
    pub fn last_typing_update(
        &self,
        room_id: &RoomId,
    ) -> Result<u64> {
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
    #[tracing::instrument(skip(self, since, _rooms, _globals))]
    pub fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<HashMap<Box<UserId>, PresenceEvent>> {
        self.db.presence_since(room_id, since)
    }
}
