use ruma::RoomId;

use crate::{database::KeyValueDatabase, service, services, Result, utils, Error};

impl service::rooms::metadata::Data for KeyValueDatabase {
    fn exists(&self, room_id: &RoomId) -> Result<bool> {
        let prefix = match services().rooms.short.get_shortroomid(room_id)? {
            Some(b) => b.to_be_bytes().to_vec(),
            None => return Ok(false),
        };

        // Look for PDUs in that room.
        Ok(self
            .pduid_pdu
            .iter_from(&prefix, false)
            .next()
            .filter(|(k, _)| k.starts_with(&prefix))
            .is_some())
    }

    fn iter_ids<'a>(&'a self) -> Box<dyn Iterator<Item = Result<Box<RoomId>>> + 'a> {
        Box::new(self.roomid_shortroomid.iter().map(|(bytes, _)| {
            RoomId::parse(
                utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Room ID in publicroomids is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Room ID in roomid_shortroomid is invalid."))
        }))

    }

    fn is_disabled(&self, room_id: &RoomId) -> Result<bool> {
        Ok(self.disabledroomids.get(room_id.as_bytes())?.is_some())
    }

    fn disable_room(&self, room_id: &RoomId, disabled: bool) -> Result<()> {
        if disabled {
            self.disabledroomids.insert(room_id.as_bytes(), &[])?;
        } else {
            self.disabledroomids.remove(room_id.as_bytes())?;
        }

        Ok(())
    }
}
