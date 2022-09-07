use ruma::{UserId, RoomId};

use crate::{service, database::KeyValueDatabase, utils, Error, Result};

impl service::rooms::user::Data for KeyValueDatabase {
    fn reset_notification_counts(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_notificationcount
            .insert(&userroom_id, &0_u64.to_be_bytes())?;
        self.userroomid_highlightcount
            .insert(&userroom_id, &0_u64.to_be_bytes())?;

        Ok(())
    }

    fn notification_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_notificationcount
            .get(&userroom_id)?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid notification count in db."))
            })
            .unwrap_or(Ok(0))
    }

    fn highlight_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_highlightcount
            .get(&userroom_id)?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid highlight count in db."))
            })
            .unwrap_or(Ok(0))
    }

    fn associate_token_shortstatehash(
        &self,
        room_id: &RoomId,
        token: u64,
        shortstatehash: u64,
    ) -> Result<()> {
        let shortroomid = self.get_shortroomid(room_id)?.expect("room exists");

        let mut key = shortroomid.to_be_bytes().to_vec();
        key.extend_from_slice(&token.to_be_bytes());

        self.roomsynctoken_shortstatehash
            .insert(&key, &shortstatehash.to_be_bytes())
    }

    fn get_token_shortstatehash(&self, room_id: &RoomId, token: u64) -> Result<Option<u64>> {
        let shortroomid = self.get_shortroomid(room_id)?.expect("room exists");

        let mut key = shortroomid.to_be_bytes().to_vec();
        key.extend_from_slice(&token.to_be_bytes());

        self.roomsynctoken_shortstatehash
            .get(&key)?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid shortstatehash in roomsynctoken_shortstatehash")
                })
            })
            .transpose()
    }

    fn get_shared_rooms<'a>(
        &'a self,
        users: Vec<Box<UserId>>,
    ) -> Result<Box<dyn Iterator<Item = Result<Box<RoomId>>>>> {
        let iterators = users.into_iter().map(move |user_id| {
            let mut prefix = user_id.as_bytes().to_vec();
            prefix.push(0xff);

            self.userroomid_joined
                .scan_prefix(prefix)
                .map(|(key, _)| {
                    let roomid_index = key
                        .iter()
                        .enumerate()
                        .find(|(_, &b)| b == 0xff)
                        .ok_or_else(|| Error::bad_database("Invalid userroomid_joined in db."))?
                        .0
                        + 1; // +1 because the room id starts AFTER the separator

                    let room_id = key[roomid_index..].to_vec();

                    Ok::<_, Error>(room_id)
                })
                .filter_map(|r| r.ok())
        });

        // We use the default compare function because keys are sorted correctly (not reversed)
        Ok(utils::common_elements(iterators, Ord::cmp)
            .expect("users is not empty")
            .map(|bytes| {
                RoomId::parse(utils::string_from_bytes(&*bytes).map_err(|_| {
                    Error::bad_database("Invalid RoomId bytes in userroomid_joined")
                })?)
                .map_err(|_| Error::bad_database("Invalid RoomId in userroomid_joined."))
            }))
    }
}
