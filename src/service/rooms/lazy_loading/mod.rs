mod data;
use std::collections::HashSet;

pub use data::Data;
use ruma::{DeviceId, UserId, RoomId};

use crate::service::*;

pub struct Service<D: Data> {
    db: D,
}

impl Service<_> {
    #[tracing::instrument(skip(self))]
    pub fn lazy_load_was_sent_before(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        ll_user: &UserId,
    ) -> Result<bool> {
        self.db.lazy_load_was_sent_before(user_id, device_id, room_id, ll_user)
    }

    #[tracing::instrument(skip(self))]
    pub fn lazy_load_mark_sent(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        lazy_load: HashSet<Box<UserId>>,
        count: u64,
    ) {
        self.lazy_load_waiting.lock().unwrap().insert(
            (
                user_id.to_owned(),
                device_id.to_owned(),
                room_id.to_owned(),
                count,
            ),
            lazy_load,
        );
    }

    #[tracing::instrument(skip(self))]
    pub fn lazy_load_confirm_delivery(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<()> {
        self.db.lazy_load_confirm_delivery(user_id, device_id, room_id, since)
    }

    #[tracing::instrument(skip(self))]
    pub fn lazy_load_reset(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
    ) -> Result<()> {
        self.db.lazy_load_reset(user_id, device_id, room_id)
    }
}
