mod data;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

pub use data::Data;
use ruma::{DeviceId, RoomId, UserId};

use crate::Result;

pub struct Service {
    pub db: &'static dyn Data,

    pub lazy_load_waiting:
        Mutex<HashMap<(Box<UserId>, Box<DeviceId>, Box<RoomId>, u64), HashSet<Box<UserId>>>>,
}

impl Service {
    #[tracing::instrument(skip(self))]
    pub fn lazy_load_was_sent_before(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        ll_user: &UserId,
    ) -> Result<bool> {
        self.db
            .lazy_load_was_sent_before(user_id, device_id, room_id, ll_user)
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
        if let Some(user_ids) = self.lazy_load_waiting.lock().unwrap().remove(&(
            user_id.to_owned(),
            device_id.to_owned(),
            room_id.to_owned(),
            since,
        )) {
            self.db.lazy_load_confirm_delivery(
                user_id,
                device_id,
                room_id,
                &mut user_ids.iter().map(|u| &**u),
            )?;
        } else {
            // Ignore
        }

        Ok(())
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
