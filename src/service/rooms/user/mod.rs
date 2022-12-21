mod data;

pub use data::Data;
use ruma::{
    events::{
        push_rules::PushRulesEvent, room::power_levels::RoomPowerLevelsEventContent,
        GlobalAccountDataEventType, StateEventType,
    },
    push::{Action, Ruleset, Tweak},
    OwnedRoomId, OwnedUserId, RoomId, UserId,
};

use crate::{services, Error, Result};

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    pub fn update_notification_counts(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let power_levels: RoomPowerLevelsEventContent = services()
            .rooms
            .state_accessor
            .room_state_get(room_id, &StateEventType::RoomPowerLevels, "")?
            .map(|ev| {
                serde_json::from_str(ev.content.get())
                    .map_err(|_| Error::bad_database("invalid m.room.power_levels event"))
            })
            .transpose()?
            .unwrap_or_default();

        let read_event = services()
            .rooms
            .edus
            .read_receipt
            .private_read_get(room_id, user_id)
            .unwrap_or(None)
            .unwrap_or(0u64);
        let mut notification_count = 0u64;
        let mut highlight_count = 0u64;

        services()
            .rooms
            .timeline
            .pdus_since(user_id, room_id, read_event)?
            .filter_map(|pdu| pdu.ok())
            .map(|(_, pdu)| pdu)
            .filter(|pdu| {
                // Don't include user's own messages in notification counts
                user_id != pdu.sender
                    && services()
                        .rooms
                        .short
                        .get_or_create_shorteventid(&pdu.event_id)
                        .unwrap_or(0)
                        != read_event
            })
            .filter_map(|pdu| {
                let rules_for_user = services()
                    .account_data
                    .get(
                        None,
                        user_id,
                        GlobalAccountDataEventType::PushRules.to_string().into(),
                    )
                    .ok()?
                    .map(|event| {
                        serde_json::from_str::<PushRulesEvent>(event.get())
                            .map_err(|_| Error::bad_database("Invalid push rules event in db."))
                    })
                    .transpose()
                    .ok()?
                    .map(|ev: PushRulesEvent| ev.content.global)
                    .unwrap_or_else(|| Ruleset::server_default(user_id));

                let mut highlight = false;
                let mut notify = false;

                for action in services()
                    .pusher
                    .get_actions(
                        user_id,
                        &rules_for_user,
                        &power_levels,
                        &pdu.to_sync_room_event(),
                        &pdu.room_id,
                    )
                    .ok()?
                {
                    match action {
                        Action::DontNotify => notify = false,
                        // TODO: Implement proper support for coalesce
                        Action::Notify | Action::Coalesce => notify = true,
                        Action::SetTweak(Tweak::Highlight(true)) => {
                            highlight = true;
                        }
                        _ => {}
                    };
                }

                if notify {
                    notification_count += 1;
                };

                if highlight {
                    highlight_count += 1;
                };

                Some(())
            })
            .for_each(|_| {});

        self.db
            .update_notification_counts(user_id, room_id, notification_count, highlight_count)
    }

    pub fn notification_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        self.db.notification_count(user_id, room_id)
    }

    pub fn highlight_count(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        self.db.highlight_count(user_id, room_id)
    }

    pub fn last_notification_read(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        self.db.last_notification_read(user_id, room_id)
    }

    pub fn associate_token_shortstatehash(
        &self,
        room_id: &RoomId,
        token: u64,
        shortstatehash: u64,
    ) -> Result<()> {
        self.db
            .associate_token_shortstatehash(room_id, token, shortstatehash)
    }

    pub fn get_token_shortstatehash(&self, room_id: &RoomId, token: u64) -> Result<Option<u64>> {
        self.db.get_token_shortstatehash(room_id, token)
    }

    pub fn get_shared_rooms(
        &self,
        users: Vec<OwnedUserId>,
    ) -> Result<impl Iterator<Item = Result<OwnedRoomId>>> {
        self.db.get_shared_rooms(users)
    }
}
