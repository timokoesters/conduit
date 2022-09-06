mod data;
pub use data::Data;
use ruma::RoomId;

pub struct Service<D: Data> {
    db: D,
}

impl Service<_> {
    #[tracing::instrument(skip(self))]
    pub fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<Option<(impl Iterator<Item = Vec<u8>> + 'a, Vec<String>)>> {
        self.db.search_pdus(room_id, search_string)
    }
}
