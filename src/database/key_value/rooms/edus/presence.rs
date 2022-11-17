use futures_util::{stream::FuturesUnordered, StreamExt};
use std::{collections::HashMap, time::Duration};

use ruma::{
    events::presence::PresenceEvent, presence::PresenceState, OwnedUserId, RoomId, UInt, UserId,
};
use tokio::{sync::mpsc, time::sleep};

use crate::{
    database::KeyValueDatabase, service, services, utils, utils::u64_from_bytes, Error, Result,
};
use crate::utils::millis_since_unix_epoch;

pub struct PresenceUpdate {
    count: u64,
    timestamp: u64,
}

impl PresenceUpdate {
    fn to_be_bytes(&self) -> &[u8] {
        &*([self.count.to_be_bytes(), self.timestamp.to_be_bytes()].concat())
    }

    fn from_be_bytes(bytes: &[u8]) -> Result<Self> {
        let (count_bytes, timestamp_bytes) = bytes.split_at(bytes.len() / 2);
        Ok(Self {
            count: u64_from_bytes(count_bytes)?,
            timestamp: u64_from_bytes(timestamp_bytes)?,
        })
    }
}

impl service::rooms::edus::presence::Data for KeyValueDatabase {
    fn update_presence(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        presence: PresenceEvent,
    ) -> Result<()> {
        let mut roomuser_id = [room_id.as_bytes(), 0xff, user_id.as_bytes()].concat();

        self.roomuserid_presenceevent.insert(
            &roomuser_id,
            &serde_json::to_vec(&presence)?,
        )?;

        self.userid_presenceupdate.insert(
            user_id.as_bytes(),
            PresenceUpdate {
                count: services().globals.next_count()?,
                timestamp: millis_since_unix_epoch(),
            }.to_be_bytes(),
        )?;

        Ok(())
    }

    fn ping_presence(&self, user_id: &UserId) -> Result<()> {
        self.userid_presenceupdate.insert(
            user_id.as_bytes(),
            PresenceUpdate {
                count: services().globals.current_count()?,
                timestamp: millis_since_unix_epoch(),
            }.to_be_bytes()
        )?;

        Ok(())
    }

    fn last_presence_update(&self, user_id: &UserId) -> Result<Option<u64>> {
        self.userid_presenceupdate
            .get(user_id.as_bytes())?
            .map(|bytes| {
                PresenceUpdate::from_be_bytes(bytes)?.timestamp
            })
            .transpose()
    }

    fn get_presence_event(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        presence_timestamp: u64
    ) -> Result<Option<PresenceEvent>> {
        let mut roomuser_id = [room_id.as_bytes(), 0xff, user_id.as_bytes()].concat();
        self.roomuserid_presenceevent
            .get(&roomuser_id)?
            .map(|value| parse_presence_event(&value, presence_timestamp))
            .transpose()
    }

    fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<Box<dyn Iterator<Item=(&UserId, PresenceEvent)>>> {
        let services = &services();
        let mut user_timestamp: HashMap<UserId, u64> = self.userid_presenceupdate
            .iter()
            .map(|(user_id_bytes, update_bytes)| (UserId::parse(utils::string_from_bytes(user_id_bytes)), PresenceUpdate::from_be_bytes(update_bytes)?))
            .filter_map(|(user_id, presence_update)| {
                if presence_update.count <= since || !services.rooms.state_cache.is_joined(user_id, room_id)? {
                    return None
                }

                Some((user_id, presence_update.timestamp))
            })
            .collect();

        Ok(
            self.roomuserid_presenceevent
                .iter()
                .filter_map(|user_id_bytes, presence_bytes| (UserId::parse(utils::string_from_bytes(user_id_bytes)), presence_bytes))
                .filter_map(|user_id, presence_bytes| {
                    let timestamp = user_timestamp.get(user_id)?;

                    Some((user_id, parse_presence_event(presence_bytes, *timestamp)?))
                })
                .into_iter()
        )
    }

    fn presence_maintain(
        &self,
        mut timer_receiver: mpsc::UnboundedReceiver<Box<UserId>>,
    ) -> Result<()> {
        let mut timers = FuturesUnordered::new();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(_user_id) = timers.next() => {
                        // TODO: Handle presence timeouts
                    }
                    Some(user_id) = timer_receiver.recv() => {
                        // Idle timeout
                        timers.push(create_presence_timer(Duration::from_secs(60), user_id.clone()));

                        // Offline timeout
                        timers.push(create_presence_timer(Duration::from_secs(60*15) , user_id));
                    }
                }
            }
        });

        Ok(())
    }
}

async fn create_presence_timer(duration: Duration, user_id: Box<UserId>) -> Box<UserId> {
    sleep(duration).await;

    user_id
}

fn parse_presence_event(bytes: &[u8], presence_timestamp: u64) -> Result<PresenceEvent> {
    let mut presence: PresenceEvent = serde_json::from_slice(bytes)
        .map_err(|_| Error::bad_database("Invalid presence event in db."))?;

    translate_active_ago(&mut presence, presence_timestamp);

    Ok(presence)
}

fn determine_presence_state(
    last_active_ago: u64,
) -> PresenceState {
    let globals = &services().globals;

    return if last_active_ago < globals.presence_idle_timeout() {
        PresenceState::Online
    } else if last_active_ago < globals.presence_offline_timeout() {
        PresenceState::Unavailable
    } else {
        PresenceState::Offline
    };
}

/// Translates the timestamp representing last_active_ago to a diff from now.
fn translate_active_ago(
    presence_event: &mut PresenceEvent,
    last_active_ts: u64,
) {
    let last_active_ago = millis_since_unix_epoch().saturating_sub(last_active_ts);

    presence_event.content.presence = determine_presence_state(last_active_ago);

    presence_event.content.last_active_ago = match presence_event.content.presence {
        PresenceState::Online => None,
        _ => Some(UInt::new_saturating(last_active_ago)),
    }
}
