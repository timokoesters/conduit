use crate::Result;
use ruma::{DeviceId, RoomId, UserId};

pub trait Data: Send + Sync {
    fn lazy_load_was_sent_before(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        ll_user: &UserId,
    ) -> Result<bool>;

    fn lazy_load_confirm_delivery(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        confirmed_user_ids: &mut dyn Iterator<Item = &UserId>,
    ) -> Result<()>;

    fn lazy_load_reset(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
    ) -> Result<()>;
}
