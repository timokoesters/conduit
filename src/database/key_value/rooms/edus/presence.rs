use futures_util::{stream::FuturesUnordered, StreamExt};
use ruma::user_id;
use std::{collections::{HashMap, hash_map::Entry}, time::Duration, mem};
use tracing::{error, info};

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
    prev_timestamp: u64,
    curr_timestamp: u64,
}

impl PresenceUpdate {
    fn to_be_bytes(&self) -> Vec<u8> {
        [self.count.to_be_bytes(), self.prev_timestamp.to_be_bytes(), self.curr_timestamp.to_be_bytes()].concat()
    }

    fn from_be_bytes(bytes: &[u8]) -> Result<Self> {
        let (count_bytes, timestamps_bytes) = bytes.split_at(mem::size_of::<u64>());
        let (prev_timestamp_bytes, curr_timestamp_bytes) = timestamps_bytes.split_at(mem::size_of::<u64>());
        Ok(Self {
            count: u64_from_bytes(count_bytes).expect("count bytes from DB are valid"),
            prev_timestamp: u64_from_bytes(prev_timestamp_bytes).expect("timestamp bytes from DB are valid"),
            curr_timestamp: u64_from_bytes(curr_timestamp_bytes).expect("timestamp bytes from DB are valid"),
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

        let timestamp = match presence.content.last_active_ago {
            Some(active_ago) => millis_since_unix_epoch().saturating_sub(active_ago.into()),
            None => millis_since_unix_epoch(),
        };

        self.userid_presenceupdate.insert(
            user_id.as_bytes(),
            &*PresenceUpdate {
                count: services().globals.next_count()?,
                prev_timestamp: timestamp,
                curr_timestamp: timestamp,
            }
            .to_be_bytes(),
        )?;

        Ok(())
    }

    fn ping_presence(&self, user_id: &UserId, update_count: bool, update_timestamp: bool) -> Result<()> {
        let now = millis_since_unix_epoch();

        let presence = self.userid_presenceupdate
            .get(user_id.as_bytes())?
            .map(|presence_bytes| PresenceUpdate::from_be_bytes(&presence_bytes))
            .transpose()?;

        let new_presence = match presence {
            Some(presence) => {
                PresenceUpdate {
                    count: if update_count { services().globals.next_count()? } else { presence.count },
                    prev_timestamp: if update_timestamp { presence.curr_timestamp } else { presence.prev_timestamp },
                    curr_timestamp: if update_timestamp { now } else { presence.curr_timestamp }
                }
            },
            None => PresenceUpdate {
                count: services().globals.current_count()?,
                prev_timestamp: now,
                curr_timestamp: now,
            }
        };

        self.userid_presenceupdate.insert(
            user_id.as_bytes(),
            &*new_presence.to_be_bytes(),
        )?;

        Ok(())
    }

    fn last_presence_update(&self, user_id: &UserId) -> Result<Option<(u64, u64)>> {
        self.userid_presenceupdate
            .get(user_id.as_bytes())?
        .map(|bytes| PresenceUpdate::from_be_bytes(&bytes).map(|update| (update.prev_timestamp, update.curr_timestamp)))
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
        let user_timestamp: HashMap<OwnedUserId, u64> = self
            .userid_presenceupdate
            .iter()
            .filter_map(|(user_id_bytes, update_bytes)| {
                Some((
                    UserId::parse(
                        utils::string_from_bytes(&user_id_bytes)
                                .expect("UserID bytes are a valid string")
                    ).expect("UserID bytes from database are a valid UserID"),
                    PresenceUpdate::from_be_bytes(&update_bytes)
                                    .expect("PresenceUpdate bytes from database are a valid PresenceUpdate"),
                ))
            })
            .filter_map(|(user_id, presence_update)| {
                if presence_update.count <= since
                   || !services()
                        .rooms
                        .state_cache
                        .is_joined(&user_id, room_id)
                        .ok()?
                {
                    return None;
                }

                Some((user_id, presence_update.curr_timestamp))
            })
            .collect();

        Ok(Box::new(
            self.roomuserid_presenceevent
                .scan_prefix(room_id.as_bytes().to_vec())
                .filter_map(|(roomuserid_bytes, presence_bytes)| {
                    let user_id_bytes = roomuserid_bytes.split(|byte| *byte == 0xff as u8).last()?;
                    Some((
                        UserId::parse(
                                utils::string_from_bytes(&user_id_bytes)
                                .expect("UserID bytes are a valid string")
                        ).expect("UserID bytes from database are a valid UserID").to_owned(),
                        presence_bytes,
                    ))
                })
                .filter_map(
                    move |(user_id, presence_bytes)| -> Option<(OwnedUserId, PresenceEvent)> {
                        let timestamp = user_timestamp.get(&user_id)?;

                        Some((
                            user_id,
                            parse_presence_event(&presence_bytes, *timestamp)
                                .expect("PresenceEvent bytes from database are a valid PresenceEvent"),
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
        let mut timers_timestamp: HashMap<OwnedUserId, u64> = HashMap::new();

        // TODO: Get rid of this hack
        timers.push(create_presence_timer(
            Duration::from_secs(60),
            user_id!("@test:test.com").to_owned(),
        ));

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(user_id) = timers.next() => {
                        info!("Processing timer for user '{}' ({})", user_id.clone(), timers.len());
                        let (prev_timestamp, curr_timestamp) = match services().rooms.edus.presence.last_presence_update(&user_id) {
                            Ok(timestamp_tuple) => match timestamp_tuple {
                                Some(timestamp_tuple) => timestamp_tuple,
                            None => continue,
                            },
                            Err(e) => {
                                error!("{e}");
                                continue;
                            }
                        };

                        let prev_presence_state = determine_presence_state(prev_timestamp);
                        let curr_presence_state = determine_presence_state(curr_timestamp);

                        // Continue if there is no change in state
                        if prev_presence_state == curr_presence_state {
                            continue;
                        }

                        match services().rooms.edus.presence.ping_presence(&user_id, true, false, false) {
                            Ok(_) => (),
                            Err(e) => error!("{e}")
                        }

                        // TODO: Notify federation sender
                    }
                    Some(user_id) = timer_receiver.recv() => {
                        let now = millis_since_unix_epoch();
                        let should_send = match timers_timestamp.entry(user_id.to_owned()) {
                            Entry::Occupied(mut entry) => {
                                if now - entry.get() > 15 * 1000 {
                                    entry.insert(now);
                                    true
                                } else {
                                    false
                                }
                            },
                        Entry::Vacant(entry) => {
                                entry.insert(now);
                                true
                            }
                        };

                        if !should_send {
                            continue;
                        }

                        // Idle timeout
                        timers.push(create_presence_timer(Duration::from_secs(60), user_id.clone()));

                        // Offline timeout
                        timers.push(create_presence_timer(Duration::from_secs(60*15) , user_id.clone()));

                        info!("Added timers for user '{}' ({})", user_id, timers.len());
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
