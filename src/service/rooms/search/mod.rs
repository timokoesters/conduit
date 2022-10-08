mod data;

pub use data::Data;

use crate::Result;
use ruma::RoomId;

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    #[tracing::instrument(skip(self))]
    pub fn index_pdu<'a>(&self, shortroomid: u64, pdu_id: &[u8], message_body: &str) -> Result<()> {
        self.db.index_pdu(shortroomid, pdu_id, message_body)
    }

    #[tracing::instrument(skip(self))]
    pub fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<Option<(impl Iterator<Item = Vec<u8>> + 'a, Vec<String>)>> {
        self.db.search_pdus(room_id, search_string)
    }
}
