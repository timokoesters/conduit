use crate::Result;
use ruma::RoomId;

pub trait Data: Send + Sync {
    fn index_pdu(&self, shortroomid: u64, pdu_id: &[u8], message_body: &str) -> Result<()>;

    #[allow(clippy::type_complexity)]
    fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<Option<(Box<dyn Iterator<Item = Vec<u8>> + 'a>, Vec<String>)>>;
}
