use std::time::Duration;

use ruma::{
    events::presence::PresenceEvent, presence::PresenceState, OwnedUserId, RoomId, UInt, UserId,
};

use crate::{
    database::KeyValueDatabase,
    service::{self, rooms::edus::presence::Presence},
    services,
    utils::{self, user_id_from_bytes},
    Error, Result,
};

impl service::rooms::edus::presence::Data for KeyValueDatabase {
    fn get_presence(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<PresenceEvent>> {
        let key = presence_key(room_id, user_id);

        self.roomuserid_presence
            .get(&key)?
            .map(|presence_bytes| -> Result<PresenceEvent> {
                Presence::from_json_bytes(&presence_bytes)?.to_presence_event(user_id)
            })
            .transpose()
    }

    fn ping_presence(&self, user_id: &UserId, new_state: PresenceState) -> Result<()> {
        let now = utils::millis_since_unix_epoch();
        let mut state_changed = false;

        for room_id in services().rooms.state_cache.rooms_joined(user_id) {
            let key = presence_key(&room_id?, user_id);

            let presence_bytes = self.roomuserid_presence.get(&key)?;

            if let Some(presence_bytes) = presence_bytes {
                let presence = Presence::from_json_bytes(&presence_bytes)?;
                if presence.state != new_state {
                    state_changed = true;
                    break;
                }
            }
        }

        let count = if state_changed {
            services().globals.next_count()?
        } else {
            services().globals.current_count()?
        };

        for room_id in services().rooms.state_cache.rooms_joined(user_id) {
            let key = presence_key(&room_id?, user_id);

            let presence_bytes = self.roomuserid_presence.get(&key)?;

            let new_presence = match presence_bytes {
                Some(presence_bytes) => {
                    let mut presence = Presence::from_json_bytes(&presence_bytes)?;
                    presence.state = new_state.clone();
                    presence.currently_active = presence.state == PresenceState::Online;
                    presence.last_active_ts = now;
                    presence.last_count = count;

                    presence
                }
                None => Presence::new(
                    new_state.clone(),
                    new_state == PresenceState::Online,
                    now,
                    count,
                    None,
                ),
            };

            self.roomuserid_presence
                .insert(&key, &new_presence.to_json_bytes()?)?;
        }

        let timeout = match new_state {
            PresenceState::Online => services().globals.config.presence_idle_timeout_s,
            _ => services().globals.config.presence_offline_timeout_s,
        };

        self.presence_timer_sender
            .send((user_id.to_owned(), Duration::from_secs(timeout)))
            .map_err(|_| Error::bad_database("Failed to add presence timer"))
    }

    fn set_presence(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        presence_state: PresenceState,
        currently_active: Option<bool>,
        last_active_ago: Option<UInt>,
        status_msg: Option<String>,
    ) -> Result<()> {
        let now = utils::millis_since_unix_epoch();
        let last_active_ts = match last_active_ago {
            Some(last_active_ago) => now.saturating_sub(last_active_ago.into()),
            None => now,
        };

        let key = presence_key(room_id, user_id);

        let presence = Presence::new(
            presence_state,
            currently_active.unwrap_or(false),
            last_active_ts,
            services().globals.next_count()?,
            status_msg,
        );

        let timeout = match presence.state {
            PresenceState::Online => services().globals.config.presence_idle_timeout_s,
            _ => services().globals.config.presence_offline_timeout_s,
        };

        self.presence_timer_sender
            .send((user_id.to_owned(), Duration::from_secs(timeout)))
            .map_err(|_| Error::bad_database("Failed to add presence timer"))?;

        self.roomuserid_presence
            .insert(&key, &presence.to_json_bytes()?)?;

        Ok(())
    }

    fn remove_presence(&self, user_id: &UserId) -> Result<()> {
        for room_id in services().rooms.state_cache.rooms_joined(user_id) {
            let key = presence_key(&room_id?, user_id);

            self.roomuserid_presence.remove(&key)?;
        }

        Ok(())
    }

    fn presence_since<'a>(
        &'a self,
        room_id: &RoomId,
        since: u64,
    ) -> Box<dyn Iterator<Item = (OwnedUserId, u64, PresenceEvent)> + 'a> {
        let prefix = [room_id.as_bytes(), &[0xff]].concat();

        Box::new(
            self.roomuserid_presence
                .scan_prefix(prefix)
                .flat_map(
                    |(key, presence_bytes)| -> Result<(OwnedUserId, u64, PresenceEvent)> {
                        let user_id = user_id_from_bytes(
                            key.rsplit(|byte| *byte == 0xff).next().ok_or_else(|| {
                                Error::bad_database("No UserID bytes in presence key")
                            })?,
                        )?;

                        let presence = Presence::from_json_bytes(&presence_bytes)?;
                        let presence_event = presence.to_presence_event(&user_id)?;

                        Ok((user_id, presence.last_count, presence_event))
                    },
                )
                .filter(move |(_, count, _)| *count > since),
        )
    }
}

#[inline]
fn presence_key(room_id: &RoomId, user_id: &UserId) -> Vec<u8> {
    [room_id.as_bytes(), &[0xff], user_id.as_bytes()].concat()
}
