use ruma::{UserId, DeviceId, RoomId};

use crate::{service, database::KeyValueDatabase};

impl service::rooms::lazy_loading::Data for KeyValueDatabase {
    fn lazy_load_was_sent_before(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        ll_user: &UserId,
    ) -> Result<bool> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(room_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(ll_user.as_bytes());
        Ok(self.lazyloadedids.get(&key)?.is_some())
    }

    fn lazy_load_confirm_delivery(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<()> {
        if let Some(user_ids) = self.lazy_load_waiting.lock().unwrap().remove(&(
            user_id.to_owned(),
            device_id.to_owned(),
            room_id.to_owned(),
            since,
        )) {
            let mut prefix = user_id.as_bytes().to_vec();
            prefix.push(0xff);
            prefix.extend_from_slice(device_id.as_bytes());
            prefix.push(0xff);
            prefix.extend_from_slice(room_id.as_bytes());
            prefix.push(0xff);

            for ll_id in user_ids {
                let mut key = prefix.clone();
                key.extend_from_slice(ll_id.as_bytes());
                self.lazyloadedids.insert(&key, &[])?;
            }
        }

        Ok(())
    }

    fn lazy_load_reset(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
    ) -> Result<()> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(device_id.as_bytes());
        prefix.push(0xff);
        prefix.extend_from_slice(room_id.as_bytes());
        prefix.push(0xff);

        for (key, _) in self.lazyloadedids.scan_prefix(prefix) {
            self.lazyloadedids.remove(&key)?;
        }

        Ok(())
    }
}
