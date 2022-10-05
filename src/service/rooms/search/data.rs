use ruma::RoomId;
use crate::Result;

pub trait Data: Send + Sync {
    fn index_pdu<'a>(&self, shortroomid: u64, pdu_id: &[u8], message_body: String) -> Result<()>;

    fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<Option<(Box<dyn Iterator<Item = Vec<u8>>>, Vec<String>)>>;
}
