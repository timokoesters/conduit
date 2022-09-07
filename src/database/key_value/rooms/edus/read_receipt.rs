use std::mem;

use ruma::{UserId, RoomId, events::receipt::ReceiptEvent, serde::Raw, signatures::CanonicalJsonObject};

use crate::{database::KeyValueDatabase, service, utils, Error, services, Result};

impl service::rooms::edus::read_receipt::Data for KeyValueDatabase {
    fn readreceipt_update(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        event: ReceiptEvent,
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
        room_latest_id.extend_from_slice(&services().globals.next_count()?.to_be_bytes());
        room_latest_id.push(0xff);
        room_latest_id.extend_from_slice(user_id.as_bytes());

        self.readreceiptid_readreceipt.insert(
            &room_latest_id,
            &serde_json::to_vec(&event).expect("EduEvent::to_string always works"),
        )?;

        Ok(())
    }

    fn readreceipts_since<'a>(
        &'a self,
        room_id: &RoomId,
        since: u64,
    ) -> Box<dyn Iterator<
        Item=Result<(
            Box<UserId>,
            u64,
            Raw<ruma::events::AnySyncEphemeralRoomEvent>,
        )>,
    >> {
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
                let user_id = UserId::parse(
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

    fn private_read_set(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        count: u64,
    ) -> Result<()> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_privateread
            .insert(&key, &count.to_be_bytes())?;

        self.roomuserid_lastprivatereadupdate
            .insert(&key, &services().globals.next_count()?.to_be_bytes())
    }

    fn private_read_get(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
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

    fn last_privateread_update(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        Ok(self
            .roomuserid_lastprivatereadupdate
            .get(&key)?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Count in roomuserid_lastprivatereadupdate is invalid.")
                })
            })
            .transpose()?
            .unwrap_or(0))
    }
}
