use ruma::RoomId;

use crate::{database::KeyValueDatabase, service, services, Result};

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
