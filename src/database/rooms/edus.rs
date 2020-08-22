use crate::{utils, Error, Result};
use js_int::UInt;
use ruma::{
    events::{
        presence::{PresenceEvent, PresenceEventContent},
        AnyEvent as EduEvent, SyncEphemeralRoomEvent,
    },
    presence::PresenceState,
    Raw, RoomId, UserId,
};
use sled::transaction::Transactional;
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
};

pub struct RoomEdus {
    pub(in super::super) roomuserid_lastread: sled::Tree, // RoomUserId = Room + User
    pub(in super::super) roomlatestid_roomlatest: sled::Tree, // Read Receipts, RoomLatestId = RoomId + Count + UserId
    pub(in super::super) roomactiveid_userid: sled::Tree, // Typing, RoomActiveId = RoomId + TimeoutTime + Count
    pub(in super::super) roomid_lastroomactiveupdate: sled::Tree, // LastRoomActiveUpdate = Count
    pub(in super::super) presenceid_presence: sled::Tree, // PresenceId = RoomId + Count + UserId
    pub(in super::super) userid_lastpresenceupdate: sled::Tree, // LastPresenceUpdate = Count
}

impl RoomEdus {
    /// Adds an event which will be saved until a new event replaces it (e.g. read receipt).
    pub fn roomlatest_update(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        event: EduEvent,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        // Remove old entry
        if let Some(old) = self
            .roomlatestid_roomlatest
            .scan_prefix(&prefix)
            .keys()
            .rev()
            .filter_map(|r| r.ok())
            .take_while(|key| key.starts_with(&prefix))
            .find(|key| {
                key.rsplit(|&b| b == 0xff)
                    .next()
                    .expect("rsplit always returns an element")
                    == user_id.to_string().as_bytes()
            })
        {
            // This is the old room_latest
            self.roomlatestid_roomlatest.remove(old)?;
        }

        let mut room_latest_id = prefix;
        room_latest_id.extend_from_slice(&globals.next_count()?.to_be_bytes());
        room_latest_id.push(0xff);
        room_latest_id.extend_from_slice(&user_id.to_string().as_bytes());

        self.roomlatestid_roomlatest.insert(
            room_latest_id,
            &*serde_json::to_string(&event).expect("EduEvent::to_string always works"),
        )?;

        Ok(())
    }

    /// Returns an iterator over the most recent read_receipts in a room that happened after the event with id `since`.
    pub fn roomlatests_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<impl Iterator<Item = Result<Raw<ruma::events::AnySyncEphemeralRoomEvent>>>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut first_possible_edu = prefix.clone();
        first_possible_edu.extend_from_slice(&(since + 1).to_be_bytes()); // +1 so we don't send the event at since

        Ok(self
            .roomlatestid_roomlatest
            .range(&*first_possible_edu..)
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(|(_, v)| {
                Ok(serde_json::from_slice(&v).map_err(|_| {
                    Error::bad_database("Read receipt in roomlatestid_roomlatest is invalid.")
                })?)
            }))
    }

    /// Sets a user as typing until the timeout timestamp is reached or roomactive_remove is
    /// called.
    pub fn roomactive_add(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        timeout: u64,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let count = globals.next_count()?.to_be_bytes();

        let mut room_active_id = prefix;
        room_active_id.extend_from_slice(&timeout.to_be_bytes());
        room_active_id.push(0xff);
        room_active_id.extend_from_slice(&count);

        (&self.roomactiveid_userid, &self.roomid_lastroomactiveupdate).transaction(
            |(roomactiveid_userid, roomid_lastroomactiveupdate)| {
                // roomactiveid_userid.insert(&room_active_id, &*user_id.to_string().as_bytes())?;
                // roomid_lastroomactiveupdate.insert(&room_id.to_string().as_bytes(), &count)?;
                roomactiveid_userid
                    .insert(room_active_id.as_slice(), &*user_id.to_string().as_bytes())?;
                roomid_lastroomactiveupdate.insert(room_id.to_string().as_bytes(), &count)?;
                Ok(())
            },
        )?;

        Ok(())
    }

    /// Removes a user from typing before the timeout is reached.
    pub fn roomactive_remove(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let user_id = user_id.to_string();

        let mut found_outdated = false;

        // Maybe there are multiple ones from calling roomactive_add multiple times
        for outdated_edu in self
            .roomactiveid_userid
            .scan_prefix(&prefix)
            .filter_map(|r| r.ok())
            .filter(|(_, v)| v == user_id.as_bytes())
        {
            self.roomactiveid_userid.remove(outdated_edu.0)?;
            found_outdated = true;
        }

        if found_outdated {
            self.roomid_lastroomactiveupdate.insert(
                &room_id.to_string().as_bytes(),
                &globals.next_count()?.to_be_bytes(),
            )?;
        }

        Ok(())
    }

    /// Makes sure that typing events with old timestamps get removed.
    fn roomactives_maintain(
        &self,
        room_id: &RoomId,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let current_timestamp = utils::millis_since_unix_epoch();

        let mut found_outdated = false;

        // Find all outdated edus before inserting a new one
        for outdated_edu in self
            .roomactiveid_userid
            .scan_prefix(&prefix)
            .keys()
            .map(|key| {
                let key = key?;
                Ok::<_, Error>((
                    key.clone(),
                    utils::u64_from_bytes(key.split(|&b| b == 0xff).nth(1).ok_or_else(|| {
                        Error::bad_database("RoomActive has invalid timestamp or delimiters.")
                    })?)
                    .map_err(|_| Error::bad_database("RoomActive has invalid timestamp bytes."))?,
                ))
            })
            .filter_map(|r| r.ok())
            .take_while(|&(_, timestamp)| timestamp < current_timestamp)
        {
            // This is an outdated edu (time > timestamp)
            self.roomactiveid_userid.remove(outdated_edu.0)?;
            found_outdated = true;
        }

        if found_outdated {
            self.roomid_lastroomactiveupdate.insert(
                &room_id.to_string().as_bytes(),
                &globals.next_count()?.to_be_bytes(),
            )?;
        }

        Ok(())
    }

    /// Returns an iterator over all active events (e.g. typing notifications).
    pub fn last_roomactive_update(
        &self,
        room_id: &RoomId,
        globals: &super::super::globals::Globals,
    ) -> Result<u64> {
        self.roomactives_maintain(room_id, globals)?;

        Ok(self
            .roomid_lastroomactiveupdate
            .get(&room_id.to_string().as_bytes())?
            .map_or(Ok::<_, Error>(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Count in roomid_lastroomactiveupdate is invalid.")
                })?))
            })?
            .unwrap_or(0))
    }

    pub fn roomactives_all(
        &self,
        room_id: &RoomId,
    ) -> Result<SyncEphemeralRoomEvent<ruma::events::typing::TypingEventContent>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut user_ids = Vec::new();

        for user_id in self
            .roomactiveid_userid
            .scan_prefix(prefix)
            .values()
            .map(|user_id| {
                Ok::<_, Error>(
                    UserId::try_from(utils::string_from_bytes(&user_id?).map_err(|_| {
                        Error::bad_database("User ID in roomactiveid_userid is invalid unicode.")
                    })?)
                    .map_err(|_| {
                        Error::bad_database("User ID in roomactiveid_userid is invalid.")
                    })?,
                )
            })
        {
            user_ids.push(user_id?);
        }

        Ok(SyncEphemeralRoomEvent {
            content: ruma::events::typing::TypingEventContent { user_ids },
        })
    }

    /// Sets a private read marker at `count`.
    pub fn room_read_set(&self, room_id: &RoomId, user_id: &UserId, count: u64) -> Result<()> {
        let mut key = room_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&user_id.to_string().as_bytes());

        self.roomuserid_lastread.insert(key, &count.to_be_bytes())?;

        Ok(())
    }

    /// Returns the private read marker.
    pub fn room_read_get(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        let mut key = room_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&user_id.to_string().as_bytes());

        self.roomuserid_lastread.get(key)?.map_or(Ok(None), |v| {
            Ok(Some(utils::u64_from_bytes(&v).map_err(|_| {
                Error::bad_database("Invalid private read marker bytes")
            })?))
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

        let mut presence_id = room_id.to_string().as_bytes().to_vec();
        presence_id.push(0xff);
        presence_id.extend_from_slice(&count);
        presence_id.push(0xff);
        presence_id.extend_from_slice(&presence.sender.to_string().as_bytes());

        self.presenceid_presence.insert(
            presence_id,
            &*serde_json::to_string(&presence).expect("PresenceEvent can be serialized"),
        )?;

        self.userid_lastpresenceupdate.insert(
            &user_id.to_string().as_bytes(),
            &utils::millis_since_unix_epoch().to_be_bytes(),
        )?;

        Ok(())
    }

    /// Resets the presence timeout, so the user will stay in their current presence state.
    pub fn ping_presence(&self, user_id: &UserId) -> Result<()> {
        self.userid_lastpresenceupdate.insert(
            &user_id.to_string().as_bytes(),
            &utils::millis_since_unix_epoch().to_be_bytes(),
        )?;

        Ok(())
    }

    /// Returns the timestamp of the last presence update of this user in millis since the unix epoch.
    pub fn last_presence_update(&self, user_id: &UserId) -> Result<Option<u64>> {
        self.userid_lastpresenceupdate
            .get(&user_id.to_string().as_bytes())?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid timestamp in userid_lastpresenceupdate.")
                })
            })
            .transpose()
    }

    /// Sets all users to offline who have been quiet for too long.
    pub fn presence_maintain(
        &self,
        rooms: &super::Rooms,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let current_timestamp = utils::millis_since_unix_epoch();

        for (user_id_bytes, last_timestamp) in self
            .userid_lastpresenceupdate
            .iter()
            .filter_map(|r| r.ok())
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
            .take_while(|(_, timestamp)| current_timestamp - timestamp > 5 * 60_000)
        // 5 Minutes
        {
            self.userid_lastpresenceupdate.remove(&user_id_bytes)?;

            // Send new presence events to set the user offline
            let count = globals.next_count()?.to_be_bytes();
            let user_id = utils::string_from_bytes(&user_id_bytes)
                .map_err(|_| {
                    Error::bad_database("Invalid UserId bytes in userid_lastpresenceupdate.")
                })?
                .try_into()
                .map_err(|_| Error::bad_database("Invalid UserId in userid_lastpresenceupdate."))?;
            for room_id in rooms.rooms_joined(&user_id).filter_map(|r| r.ok()) {
                let mut presence_id = room_id.to_string().as_bytes().to_vec();
                presence_id.push(0xff);
                presence_id.extend_from_slice(&count);
                presence_id.push(0xff);
                presence_id.extend_from_slice(&user_id_bytes);

                self.presenceid_presence.insert(
                    presence_id,
                    &*serde_json::to_string(&PresenceEvent {
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
        }

        Ok(())
    }

    /// Returns an iterator over the most recent presence updates that happened after the event with id `since`.
    pub fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
        rooms: &super::Rooms,
        globals: &super::super::globals::Globals,
    ) -> Result<HashMap<UserId, PresenceEvent>> {
        self.presence_maintain(rooms, globals)?;

        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut first_possible_edu = prefix.clone();
        first_possible_edu.extend_from_slice(&(since + 1).to_be_bytes()); // +1 so we don't send the event at since
        let mut hashmap = HashMap::new();

        for (key, value) in self
            .presenceid_presence
            .range(&*first_possible_edu..)
            .filter_map(|r| r.ok())
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
