use ruma::{UserId, RoomId, events::{AnyStrippedStateEvent, AnySyncStateEvent}, serde::Raw};

use crate::{service, database::KeyValueDatabase, services, Result};

impl service::rooms::state_cache::Data for KeyValueDatabase {
    fn mark_as_once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());
        self.roomuseroncejoinedids.insert(&userroom_id, &[])
    }

    fn mark_as_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_joined.insert(&userroom_id, &[])?;
        self.roomuserid_joined.insert(&roomuser_id, &[])?;
        self.userroomid_invitestate.remove(&userroom_id)?;
        self.roomuserid_invitecount.remove(&roomuser_id)?;
        self.userroomid_leftstate.remove(&userroom_id)?;
        self.roomuserid_leftcount.remove(&roomuser_id)?;

        Ok(())
    }
    
    fn mark_as_invited(&self, user_id: &UserId, room_id: &RoomId, last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>) -> Result<()> {
        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_invitestate.insert(
            &userroom_id,
            &serde_json::to_vec(&last_state.unwrap_or_default())
                .expect("state to bytes always works"),
        )?;
        self.roomuserid_invitecount
            .insert(&roomuser_id, &services().globals.next_count()?.to_be_bytes())?;
        self.userroomid_joined.remove(&userroom_id)?;
        self.roomuserid_joined.remove(&roomuser_id)?;
        self.userroomid_leftstate.remove(&userroom_id)?;
        self.roomuserid_leftcount.remove(&roomuser_id)?;

        Ok(())
    }

    fn mark_as_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_leftstate.insert(
            &userroom_id,
            &serde_json::to_vec(&Vec::<Raw<AnySyncStateEvent>>::new()).unwrap(),
        )?; // TODO
        self.roomuserid_leftcount
            .insert(&roomuser_id, &services().globals.next_count()?.to_be_bytes())?;
        self.userroomid_joined.remove(&userroom_id)?;
        self.roomuserid_joined.remove(&roomuser_id)?;
        self.userroomid_invitestate.remove(&userroom_id)?;
        self.roomuserid_invitecount.remove(&roomuser_id)?;

        Ok(())
    }
}
