use std::sync::Arc;

use ruma::RoomId;

use crate::{service, database::KeyValueDatabase, Result, services};

impl service::rooms::metadata::Data for Arc<KeyValueDatabase> {
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
}
