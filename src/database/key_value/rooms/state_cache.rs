use ruma::{UserId, RoomId};

use crate::{service, database::KeyValueDatabase};

impl service::rooms::state_cache::Data for KeyValueDatabase {
    fn mark_as_once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());
        self.roomuseroncejoinedids.insert(&userroom_id, &[])
    }
}
