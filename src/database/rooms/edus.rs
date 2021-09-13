use crate::{database::abstraction::Tree, utils, Error, Result};
use ruma::{
    events::{
        presence::{PresenceEvent, PresenceEventContent},
        AnyEphemeralRoomEvent, SyncEphemeralRoomEvent,
    },
    presence::PresenceState,
    serde::Raw,
    signatures::CanonicalJsonObject,
    RoomId, UInt, UserId,
};
use std::{
    collections::{HashMap, HashSet},
    convert::{TryFrom, TryInto},
    mem,
    sync::Arc,
};

pub struct RoomEdus {
    pub(in super::super) readreceiptid_readreceipt: Arc<dyn Tree>, // ReadReceiptId = RoomId + Count + UserId
    pub(in super::super) roomuserid_privateread: Arc<dyn Tree>, // RoomUserId = Room + User, PrivateRead = Count
    pub(in super::super) roomuserid_lastprivatereadupdate: Arc<dyn Tree>, // LastPrivateReadUpdate = Count
    pub(in super::super) typingid_userid: Arc<dyn Tree>, // TypingId = RoomId + TimeoutTime + Count
    pub(in super::super) roomid_lasttypingupdate: Arc<dyn Tree>, // LastRoomTypingUpdate = Count
    pub(in super::super) presenceid_presence: Arc<dyn Tree>, // PresenceId = RoomId + Count + UserId
    pub(in super::super) userid_lastpresenceupdate: Arc<dyn Tree>, // LastPresenceUpdate = Count
}

impl RoomEdus {
    /// Adds an event which will be saved until a new event replaces it (e.g. read receipt).
    pub fn readreceipt_update(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        event: AnyEphemeralRoomEvent,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut last_possible_key = prefix.clone();
        last_possible_key.extend_from_slice(&u64::MAX.to_be_bytes());

        // Remove old entry
        if let Some((old, _)) = self
            .readreceiptid_readreceipt
            .iter_from(&last_possible_key, true)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .find(|(key, _)| {
                key.rsplit(|&b| b == 0xff)
                    .next()
                    .expect("rsplit always returns an element")
                    == user_id.as_bytes()
            })
        {
            // This is the old room_latest
            self.readreceiptid_readreceipt.remove(&old)?;
        }

        let mut room_latest_id = prefix;
        room_latest_id.extend_from_slice(&globals.next_count()?.to_be_bytes());
        room_latest_id.push(0xff);
        room_latest_id.extend_from_slice(user_id.as_bytes());

        self.readreceiptid_readreceipt.insert(
            &room_latest_id,
            &serde_json::to_vec(&event).expect("EduEvent::to_string always works"),
        )?;

        Ok(())
    }

    /// Returns an iterator over the most recent read_receipts in a room that happened after the event with id `since`.
    #[tracing::instrument(skip(self))]
    pub fn readreceipts_since<'a>(
        &'a self,
        room_id: &RoomId,
        since: u64,
    ) -> impl Iterator<Item = Result<(UserId, u64, Raw<ruma::events::AnySyncEphemeralRoomEvent>)>> + 'a
    {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);
        let prefix2 = prefix.clone();

        let mut first_possible_edu = prefix.clone();
        first_possible_edu.extend_from_slice(&(since + 1).to_be_bytes()); // +1 so we don't send the event at since

        self.readreceiptid_readreceipt
            .iter_from(&first_possible_edu, false)
            .take_while(move |(k, _)| k.starts_with(&prefix2))
            .map(move |(k, v)| {
                let count =
                    utils::u64_from_bytes(&k[prefix.len()..prefix.len() + mem::size_of::<u64>()])
                        .map_err(|_| Error::bad_database("Invalid readreceiptid count in db."))?;
                let user_id = UserId::try_from(
                    utils::string_from_bytes(&k[prefix.len() + mem::size_of::<u64>() + 1..])
                        .map_err(|_| {
                            Error::bad_database("Invalid readreceiptid userid bytes in db.")
                        })?,
                )
                .map_err(|_| Error::bad_database("Invalid readreceiptid userid in db."))?;

                let mut json = serde_json::from_slice::<CanonicalJsonObject>(&v).map_err(|_| {
                    Error::bad_database("Read receipt in roomlatestid_roomlatest is invalid json.")
                })?;
                json.remove("room_id");

                Ok((
                    user_id,
                    count,
                    Raw::from_json(
                        serde_json::value::to_raw_value(&json).expect("json is valid raw value"),
                    ),
                ))
            })
    }

    /// Sets a private read marker at `count`.
    #[tracing::instrument(skip(self, globals))]
    pub fn private_read_set(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        count: u64,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_privateread
            .insert(&key, &count.to_be_bytes())?;

        self.roomuserid_lastprivatereadupdate
            .insert(&key, &globals.next_count()?.to_be_bytes())?;

        Ok(())
    }

    /// Returns the private read marker.
    #[tracing::instrument(skip(self))]
    pub fn private_read_get(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_privateread
            .get(&key)?
            .map_or(Ok(None), |v| {
                Ok(Some(utils::u64_from_bytes(&v).map_err(|_| {
                    Error::bad_database("Invalid private read marker bytes")
                })?))
            })
    }

    /// Returns the count of the last typing update in this room.
    pub fn last_privateread_update(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        Ok(self
            .roomuserid_lastprivatereadupdate
            .get(&key)?
            .map_or(Ok::<_, Error>(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Count in roomuserid_lastprivatereadupdate is invalid.")
                })?))
            })?
            .unwrap_or(0))
    }

    /// Sets a user as typing until the timeout timestamp is reached or roomtyping_remove is
    /// called.
    pub fn typing_add(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        timeout: u64,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let count = globals.next_count()?.to_be_bytes();

        let mut room_typing_id = prefix;
        room_typing_id.extend_from_slice(&timeout.to_be_bytes());
        room_typing_id.push(0xff);
        room_typing_id.extend_from_slice(&count);

        self.typingid_userid
            .insert(&room_typing_id, &*user_id.as_bytes())?;

        self.roomid_lasttypingupdate
            .insert(room_id.as_bytes(), &count)?;

        Ok(())
    }

    /// Removes a user from typing before the timeout is reached.
    pub fn typing_remove(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let user_id = user_id.to_string();

        let mut found_outdated = false;

        // Maybe there are multiple ones from calling roomtyping_add multiple times
        for outdated_edu in self
            .typingid_userid
            .scan_prefix(prefix)
            .filter(|(_, v)| &**v == user_id.as_bytes())
        {
            self.typingid_userid.remove(&outdated_edu.0)?;
            found_outdated = true;
        }

        if found_outdated {
            self.roomid_lasttypingupdate
                .insert(room_id.as_bytes(), &globals.next_count()?.to_be_bytes())?;
        }

        Ok(())
    }

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

    /// Returns the count of the last typing update in this room.
    #[tracing::instrument(skip(self, globals))]
    pub fn last_typing_update(
        &self,
        room_id: &RoomId,
        globals: &super::super::globals::Globals,
    ) -> Result<u64> {
        self.typings_maintain(room_id, globals)?;

        Ok(self
            .roomid_lasttypingupdate
            .get(room_id.as_bytes())?
            .map_or(Ok::<_, Error>(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Count in roomid_lastroomactiveupdate is invalid.")
                })?))
            })?
            .unwrap_or(0))
    }

    pub fn typings_all(
        &self,
        room_id: &RoomId,
    ) -> Result<SyncEphemeralRoomEvent<ruma::events::typing::TypingEventContent>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut user_ids = HashSet::new();

        for user_id in self
            .typingid_userid
            .scan_prefix(prefix)
            .map(|(_, user_id)| {
                UserId::try_from(utils::string_from_bytes(&user_id).map_err(|_| {
                    Error::bad_database("User ID in typingid_userid is invalid unicode.")
                })?)
                .map_err(|_| Error::bad_database("User ID in typingid_userid is invalid."))
            })
        {
            user_ids.insert(user_id?);
        }

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
        presence: ruma::events::presence::PresenceEvent,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        // TODO: Remove old entry? Or maybe just wipe completely from time to time?

        let count = globals.next_count()?.to_be_bytes();

        let mut presence_id = room_id.as_bytes().to_vec();
        presence_id.push(0xff);
        presence_id.extend_from_slice(&count);
        presence_id.push(0xff);
        presence_id.extend_from_slice(presence.sender.as_bytes());

        self.presenceid_presence.insert(
            &presence_id,
            &serde_json::to_vec(&presence).expect("PresenceEvent can be serialized"),
        )?;

        self.userid_lastpresenceupdate.insert(
            user_id.as_bytes(),
            &utils::millis_since_unix_epoch().to_be_bytes(),
        )?;

        Ok(())
    }

    /// Resets the presence timeout, so the user will stay in their current presence state.
    #[tracing::instrument(skip(self))]
    pub fn ping_presence(&self, user_id: &UserId) -> Result<()> {
        self.userid_lastpresenceupdate.insert(
            user_id.as_bytes(),
            &utils::millis_since_unix_epoch().to_be_bytes(),
        )?;

        Ok(())
    }

    /// Returns the timestamp of the last presence update of this user in millis since the unix epoch.
    pub fn last_presence_update(&self, user_id: &UserId) -> Result<Option<u64>> {
        self.userid_lastpresenceupdate
            .get(user_id.as_bytes())?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid timestamp in userid_lastpresenceupdate.")
                })
            })
            .transpose()
    }

    pub fn get_last_presence_event(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<PresenceEvent>> {
        let last_update = match self.last_presence_update(user_id)? {
            Some(last) => last,
            None => return Ok(None),
        };

        let mut presence_id = room_id.as_bytes().to_vec();
        presence_id.push(0xff);
        presence_id.extend_from_slice(&last_update.to_be_bytes());
        presence_id.push(0xff);
        presence_id.extend_from_slice(user_id.as_bytes());

        self.presenceid_presence
            .get(&presence_id)?
            .map(|value| {
                let mut presence = serde_json::from_slice::<PresenceEvent>(&value)
                    .map_err(|_| Error::bad_database("Invalid presence event in db."))?;
                let current_timestamp: UInt = utils::millis_since_unix_epoch()
                    .try_into()
                    .expect("time is valid");

                if presence.content.presence == PresenceState::Online {
                    // Don't set last_active_ago when the user is online
                    presence.content.last_active_ago = None;
                } else {
                    // Convert from timestamp to duration
                    presence.content.last_active_ago = presence
                        .content
                        .last_active_ago
                        .map(|timestamp| current_timestamp - timestamp);
                }

                Ok(presence)
            })
            .transpose()
    }

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
            let user_id = utils::string_from_bytes(&user_id_bytes)
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
                        sender: user_id.clone(),
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
    }

    /// Returns an iterator over the most recent presence updates that happened after the event with id `since`.
    #[tracing::instrument(skip(self, since, _rooms, _globals))]
    pub fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
        _rooms: &super::Rooms,
        _globals: &super::super::globals::Globals,
    ) -> Result<HashMap<UserId, PresenceEvent>> {
        //self.presence_maintain(rooms, globals)?;

        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut first_possible_edu = prefix.clone();
        first_possible_edu.extend_from_slice(&(since + 1).to_be_bytes()); // +1 so we don't send the event at since
        let mut hashmap = HashMap::new();

        for (key, value) in self
            .presenceid_presence
            .iter_from(&*first_possible_edu, false)
            .take_while(|(key, _)| key.starts_with(&prefix))
        {
            let user_id = UserId::try_from(
                utils::string_from_bytes(
                    key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| Error::bad_database("Invalid UserId bytes in presenceid_presence."))?,
            )
            .map_err(|_| Error::bad_database("Invalid UserId in presenceid_presence."))?;

            let mut presence = serde_json::from_slice::<PresenceEvent>(&value)
                .map_err(|_| Error::bad_database("Invalid presence event in db."))?;

            let current_timestamp: UInt = utils::millis_since_unix_epoch()
                .try_into()
                .expect("time is valid");

            if presence.content.presence == PresenceState::Online {
                // Don't set last_active_ago when the user is online
                presence.content.last_active_ago = None;
            } else {
                // Convert from timestamp to duration
                presence.content.last_active_ago = presence
                    .content
                    .last_active_ago
                    .map(|timestamp| current_timestamp - timestamp);
            }

            hashmap.insert(user_id, presence);
        }

        Ok(hashmap)
    }
}
