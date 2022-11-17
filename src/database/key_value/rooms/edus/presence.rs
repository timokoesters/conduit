use futures_util::{stream::FuturesUnordered, StreamExt};
use std::{collections::HashMap, time::Duration};

use ruma::{
    events::presence::PresenceEvent, presence::PresenceState, OwnedUserId, RoomId, UInt, UserId,
};
use tokio::{sync::mpsc, time::sleep};

use crate::{database::KeyValueDatabase, service, services, utils, Error, Result};

impl service::rooms::edus::presence::Data for KeyValueDatabase {
    fn update_presence(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        presence: PresenceEvent,
    ) -> Result<()> {
        // TODO: Remove old entry? Or maybe just wipe completely from time to time?

        let count = services().globals.next_count()?.to_be_bytes();

        let mut presence_id = room_id.as_bytes().to_vec();
        presence_id.push(0xff);
        presence_id.extend_from_slice(&count);
        presence_id.push(0xff);
        presence_id.extend_from_slice(presence.sender.as_bytes());

        self.presenceid_presence.insert(
            &presence_id,
            &serde_json::to_vec(&presence).expect("PresenceEvent can be serialized"),
        )?;

        self.userid_lastpresenceupdate.insert(
            user_id.as_bytes(),
            &utils::millis_since_unix_epoch().to_be_bytes(),
        )?;

        Ok(())
    }

    fn ping_presence(&self, user_id: &UserId) -> Result<()> {
        self.userid_lastpresenceupdate.insert(
            user_id.as_bytes(),
            &utils::millis_since_unix_epoch().to_be_bytes(),
        )?;

        Ok(())
    }

    fn last_presence_update(&self, user_id: &UserId) -> Result<Option<u64>> {
        self.userid_lastpresenceupdate
            .get(user_id.as_bytes())?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid timestamp in userid_lastpresenceupdate.")
                })
            })
            .transpose()
    }

    fn get_presence_event(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        count: u64,
    ) -> Result<Option<PresenceEvent>> {
        let mut presence_id = room_id.as_bytes().to_vec();
        presence_id.push(0xff);
        presence_id.extend_from_slice(&count.to_be_bytes());
        presence_id.push(0xff);
        presence_id.extend_from_slice(user_id.as_bytes());

        self.presenceid_presence
            .get(&presence_id)?
            .map(|value| parse_presence_event(&value))
            .transpose()
    }

    fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<HashMap<OwnedUserId, PresenceEvent>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut first_possible_edu = prefix.clone();
        first_possible_edu.extend_from_slice(&(since + 1).to_be_bytes()); // +1 so we don't send the event at since
        let mut hashmap = HashMap::new();

        for (key, value) in self
            .presenceid_presence
            .iter_from(&first_possible_edu, false)
            .take_while(|(key, _)| key.starts_with(&prefix))
        {
            let user_id = UserId::parse(
                utils::string_from_bytes(
                    key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| Error::bad_database("Invalid UserId bytes in presenceid_presence."))?,
            )
            .map_err(|_| Error::bad_database("Invalid UserId in presenceid_presence."))?;

            let presence = parse_presence_event(&value)?;

            hashmap.insert(user_id, presence);
        }

        Ok(hashmap)
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

fn parse_presence_event(bytes: &[u8]) -> Result<PresenceEvent> {
    let mut presence: PresenceEvent = serde_json::from_slice(bytes)
        .map_err(|_| Error::bad_database("Invalid presence event in db."))?;

    let current_timestamp: UInt = utils::millis_since_unix_epoch()
        .try_into()
        .expect("time is valid");

    if presence.content.presence == PresenceState::Online {
        // Don't set last_active_ago when the user is online
        presence.content.last_active_ago = None;
    } else {
        // Convert from timestamp to duration
        presence.content.last_active_ago = presence
            .content
            .last_active_ago
            .map(|timestamp| current_timestamp - timestamp);
    }

    Ok(presence)
}
