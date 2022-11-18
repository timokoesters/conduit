use futures_util::{stream::FuturesUnordered, StreamExt};
use ruma::user_id;
use std::{collections::HashMap, time::Duration};
use tracing::error;

use ruma::{
    events::presence::PresenceEvent, presence::PresenceState, OwnedUserId, RoomId, UInt, UserId,
};
use tokio::{sync::mpsc, time::sleep};

use crate::{
    database::KeyValueDatabase,
    service, services, utils,
    utils::{millis_since_unix_epoch, u64_from_bytes},
    Error, Result,
};

pub struct PresenceUpdate {
    count: u64,
    timestamp: u64,
}

impl PresenceUpdate {
    fn to_be_bytes(&self) -> Vec<u8> {
        [self.count.to_be_bytes(), self.timestamp.to_be_bytes()].concat()
    }

    fn from_be_bytes(bytes: &[u8]) -> Result<Self> {
        let (count_bytes, timestamp_bytes) = bytes.split_at(bytes.len() / 2);
        Ok(Self {
            count: u64_from_bytes(count_bytes).expect("count bytes from DB are valid"),
            timestamp: u64_from_bytes(timestamp_bytes).expect("timestamp bytes from DB are valid"),
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
        let roomuser_id = [room_id.as_bytes(), &[0xff], user_id.as_bytes()].concat();

        self.roomuserid_presenceevent.insert(
            &roomuser_id,
            &serde_json::to_vec(&presence).expect("presence event from DB is valid"),
        )?;

        self.userid_presenceupdate.insert(
            user_id.as_bytes(),
            &*PresenceUpdate {
                count: services().globals.next_count()?,
                timestamp: match presence.content.last_active_ago {
                    Some(active_ago) => millis_since_unix_epoch().saturating_sub(active_ago.into()),
                    None => millis_since_unix_epoch(),
                },
            }
            .to_be_bytes(),
        )?;

        Ok(())
    }

    fn ping_presence(&self, user_id: &UserId) -> Result<()> {
        self.userid_presenceupdate.insert(
            user_id.as_bytes(),
            &*PresenceUpdate {
                count: services().globals.current_count()?,
                timestamp: millis_since_unix_epoch(),
            }
            .to_be_bytes(),
        )?;

        Ok(())
    }

    fn last_presence_update(&self, user_id: &UserId) -> Result<Option<u64>> {
        self.userid_presenceupdate
            .get(user_id.as_bytes())?
            .map(|bytes| PresenceUpdate::from_be_bytes(&bytes).map(|update| update.timestamp))
            .transpose()
    }

    fn get_presence_event(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        presence_timestamp: u64,
    ) -> Result<Option<PresenceEvent>> {
        let roomuser_id = [room_id.as_bytes(), &[0xff], user_id.as_bytes()].concat();
        self.roomuserid_presenceevent
            .get(&roomuser_id)?
            .map(|value| parse_presence_event(&value, presence_timestamp))
            .transpose()
    }

    fn presence_since<'a>(
        &'a self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<Box<dyn Iterator<Item = (OwnedUserId, PresenceEvent)> + 'a>> {
        let services = &services();
        let user_timestamp: HashMap<OwnedUserId, u64> = self
            .userid_presenceupdate
            .iter()
            .filter_map(|(user_id_bytes, update_bytes)| {
                Some((
                    OwnedUserId::from(
                        UserId::parse(utils::string_from_bytes(&user_id_bytes).ok()?).ok()?,
                    ),
                    PresenceUpdate::from_be_bytes(&update_bytes).ok()?,
                ))
            })
            .filter_map(|(user_id, presence_update)| {
                if presence_update.count <= since
                    || !services
                        .rooms
                        .state_cache
                        .is_joined(&user_id, room_id)
                        .ok()?
                {
                    return None;
                }

                Some((user_id, presence_update.timestamp))
            })
            .collect();

        Ok(Box::new(
            self.roomuserid_presenceevent
                .iter()
                .filter_map(|(user_id_bytes, presence_bytes)| {
                    Some((
                        OwnedUserId::from(
                            UserId::parse(utils::string_from_bytes(&user_id_bytes).ok()?).ok()?,
                        ),
                        presence_bytes,
                    ))
                })
                .filter_map(
                    move |(user_id, presence_bytes)| -> Option<(OwnedUserId, PresenceEvent)> {
                        let timestamp = user_timestamp.get(&user_id)?;

                        Some((
                            user_id,
                            parse_presence_event(&presence_bytes, *timestamp).ok()?,
                        ))
                    },
                ),
        ))
    }

    fn presence_maintain(
        &self,
        mut timer_receiver: mpsc::UnboundedReceiver<OwnedUserId>,
    ) -> Result<()> {
        let mut timers = FuturesUnordered::new();

        // TODO: Get rid of this hack
        timers.push(create_presence_timer(
            Duration::from_secs(60),
            user_id!("@test:test.com").to_owned(),
        ));

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(user_id) = timers.next() => {
                        let presence_timestamp = match services().rooms.edus.presence.last_presence_update(&user_id) {
                            Ok(timestamp) => match timestamp {
                                Some(timestamp) => timestamp,
                                None => continue,
                            },
                            Err(e) => {
                                error!("{e}");
                                continue;
                            }
                        };

                        let presence_state = determine_presence_state(presence_timestamp);

                        // Continue if there is no change in state
                        if presence_state != PresenceState::Offline {
                            continue;
                        }

                        for room_id in services()
                                        .rooms
                                        .state_cache
                                        .rooms_joined(&user_id)
                                        .filter_map(|room_id| room_id.ok()) {
                            let presence_event = match services().rooms.edus.presence.get_presence_event(&user_id, &room_id) {
                                Ok(event) => match event {
                                    Some(event) => event,
                                    None => continue,
                                },
                                Err(e) => {
                                    error!("{e}");
                                    continue;
                                }
                            };

                            match services().rooms.edus.presence.update_presence(&user_id, &room_id, presence_event) {
                                Ok(()) => (),
                                Err(e) => {
                                    error!("{e}");
                                    continue;
                                }
                            }

                            // TODO: Send event over federation
                        }
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

async fn create_presence_timer(duration: Duration, user_id: OwnedUserId) -> OwnedUserId {
    sleep(duration).await;

    user_id
}

fn parse_presence_event(bytes: &[u8], presence_timestamp: u64) -> Result<PresenceEvent> {
    let mut presence: PresenceEvent = serde_json::from_slice(bytes)
        .map_err(|_| Error::bad_database("Invalid presence event in db."))?;

    translate_active_ago(&mut presence, presence_timestamp);

    Ok(presence)
}

fn determine_presence_state(last_active_ago: u64) -> PresenceState {
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
fn translate_active_ago(presence_event: &mut PresenceEvent, last_active_ts: u64) {
    let last_active_ago = millis_since_unix_epoch().saturating_sub(last_active_ts);

    presence_event.content.presence = determine_presence_state(last_active_ago);

    presence_event.content.last_active_ago = match presence_event.content.presence {
        PresenceState::Online => None,
        _ => Some(UInt::new_saturating(last_active_ago)),
    }
}
