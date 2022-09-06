use ruma::RoomId;

pub trait Data {
    fn index_pdu<'a>(&self, room_id: &RoomId, pdu_id: u64, message_body: String) -> Result<()>;

    fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<Option<(impl Iterator<Item = Vec<u8>> + 'a, Vec<String>)>>;
}
