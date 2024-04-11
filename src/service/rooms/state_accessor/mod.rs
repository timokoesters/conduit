mod data;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub use data::Data;
use lru_cache::LruCache;
use ruma::{
    events::{
        room::{
            avatar::RoomAvatarEventContent,
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            member::{MembershipState, RoomMemberEventContent},
            name::RoomNameEventContent,
        },
        StateEventType,
    },
    EventId, JsOption, OwnedServerName, OwnedUserId, RoomId, ServerName, UserId,
};
use serde_json::value::to_raw_value;
use tracing::{error, warn};

use crate::{service::pdu::PduBuilder, services, Error, PduEvent, Result};

pub struct Service {
    pub db: &'static dyn Data,
    pub server_visibility_cache: Mutex<LruCache<(OwnedServerName, u64), bool>>,
    pub user_visibility_cache: Mutex<LruCache<(OwnedUserId, u64), bool>>,
}

impl Service {
    /// Builds a StateMap by iterating over all keys that start
    /// with state_hash, this gives the full state for the given state_hash.
    #[tracing::instrument(skip(self))]
    pub async fn state_full_ids(&self, shortstatehash: u64) -> Result<HashMap<u64, Arc<EventId>>> {
        self.db.state_full_ids(shortstatehash).await
    }

    pub async fn state_full(
        &self,
        shortstatehash: u64,
    ) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>> {
        self.db.state_full(shortstatehash).await
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn state_get_id(
        &self,
        shortstatehash: u64,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<EventId>>> {
        self.db.state_get_id(shortstatehash, event_type, state_key)
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    pub fn state_get(
        &self,
        shortstatehash: u64,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<PduEvent>>> {
        self.db.state_get(shortstatehash, event_type, state_key)
    }

    /// Get membership for given user in state
    fn user_membership(&self, shortstatehash: u64, user_id: &UserId) -> Result<MembershipState> {
        self.state_get(
            shortstatehash,
            &StateEventType::RoomMember,
            user_id.as_str(),
        )?
        .map_or(Ok(MembershipState::Leave), |s| {
            serde_json::from_str(s.content.get())
                .map(|c: RoomMemberEventContent| c.membership)
                .map_err(|_| Error::bad_database("Invalid room membership event in database."))
        })
    }

    /// The user was a joined member at this state (potentially in the past)
    fn user_was_joined(&self, shortstatehash: u64, user_id: &UserId) -> bool {
        self.user_membership(shortstatehash, user_id)
            .map(|s| s == MembershipState::Join)
            .unwrap_or_default() // Return sensible default, i.e. false
    }

    /// The user was an invited or joined room member at this state (potentially
    /// in the past)
    fn user_was_invited(&self, shortstatehash: u64, user_id: &UserId) -> bool {
        self.user_membership(shortstatehash, user_id)
            .map(|s| s == MembershipState::Join || s == MembershipState::Invite)
            .unwrap_or_default() // Return sensible default, i.e. false
    }

    /// Whether a server is allowed to see an event through federation, based on
    /// the room's history_visibility at that event's state.
    #[tracing::instrument(skip(self, origin, room_id, event_id))]
    pub fn server_can_see_event(
        &self,
        origin: &ServerName,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<bool> {
        let shortstatehash = match self.pdu_shortstatehash(event_id)? {
            Some(shortstatehash) => shortstatehash,
            None => return Ok(true),
        };

        if let Some(visibility) = self
            .server_visibility_cache
            .lock()
            .unwrap()
            .get_mut(&(origin.to_owned(), shortstatehash))
        {
            return Ok(*visibility);
        }

        let history_visibility = self
            .state_get(shortstatehash, &StateEventType::RoomHistoryVisibility, "")?
            .map_or(Ok(HistoryVisibility::Shared), |s| {
                serde_json::from_str(s.content.get())
                    .map(|c: RoomHistoryVisibilityEventContent| c.history_visibility)
                    .map_err(|_| {
                        Error::bad_database("Invalid history visibility event in database.")
                    })
            })?;

        let mut current_server_members = services()
            .rooms
            .state_cache
            .room_members(room_id)
            .filter_map(|r| r.ok())
            .filter(|member| member.server_name() == origin);

        let visibility = match history_visibility {
            HistoryVisibility::WorldReadable | HistoryVisibility::Shared => true,
            HistoryVisibility::Invited => {
                // Allow if any member on requesting server was AT LEAST invited, else deny
                current_server_members.any(|member| self.user_was_invited(shortstatehash, &member))
            }
            HistoryVisibility::Joined => {
                // Allow if any member on requested server was joined, else deny
                current_server_members.any(|member| self.user_was_joined(shortstatehash, &member))
            }
            _ => {
                error!("Unknown history visibility {history_visibility}");
                false
            }
        };

        self.server_visibility_cache
            .lock()
            .unwrap()
            .insert((origin.to_owned(), shortstatehash), visibility);

        Ok(visibility)
    }

    /// Whether a user is allowed to see an event, based on
    /// the room's history_visibility at that event's state.
    #[tracing::instrument(skip(self, user_id, room_id, event_id))]
    pub fn user_can_see_event(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<bool> {
        let shortstatehash = match self.pdu_shortstatehash(event_id)? {
            Some(shortstatehash) => shortstatehash,
            None => return Ok(true),
        };

        if let Some(visibility) = self
            .user_visibility_cache
            .lock()
            .unwrap()
            .get_mut(&(user_id.to_owned(), shortstatehash))
        {
            return Ok(*visibility);
        }

        let currently_member = services().rooms.state_cache.is_joined(user_id, room_id)?;

        let history_visibility = self
            .state_get(shortstatehash, &StateEventType::RoomHistoryVisibility, "")?
            .map_or(Ok(HistoryVisibility::Shared), |s| {
                serde_json::from_str(s.content.get())
                    .map(|c: RoomHistoryVisibilityEventContent| c.history_visibility)
                    .map_err(|_| {
                        Error::bad_database("Invalid history visibility event in database.")
                    })
            })?;

        let visibility = match history_visibility {
            HistoryVisibility::WorldReadable => true,
            HistoryVisibility::Shared => currently_member,
            HistoryVisibility::Invited => {
                // Allow if any member on requesting server was AT LEAST invited, else deny
                self.user_was_invited(shortstatehash, user_id)
            }
            HistoryVisibility::Joined => {
                // Allow if any member on requested server was joined, else deny
                self.user_was_joined(shortstatehash, user_id)
            }
            _ => {
                error!("Unknown history visibility {history_visibility}");
                false
            }
        };

        self.user_visibility_cache
            .lock()
            .unwrap()
            .insert((user_id.to_owned(), shortstatehash), visibility);

        Ok(visibility)
    }

    /// Whether a user is allowed to see an event, based on
    /// the room's history_visibility at that event's state.
    #[tracing::instrument(skip(self, user_id, room_id))]
    pub fn user_can_see_state_events(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let currently_member = services().rooms.state_cache.is_joined(user_id, room_id)?;

        let history_visibility = self
            .room_state_get(room_id, &StateEventType::RoomHistoryVisibility, "")?
            .map_or(Ok(HistoryVisibility::Shared), |s| {
                serde_json::from_str(s.content.get())
                    .map(|c: RoomHistoryVisibilityEventContent| c.history_visibility)
                    .map_err(|_| {
                        Error::bad_database("Invalid history visibility event in database.")
                    })
            })?;

        Ok(currently_member || history_visibility == HistoryVisibility::WorldReadable)
    }

    /// Returns the state hash for this pdu.
    pub fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.db.pdu_shortstatehash(event_id)
    }

    /// Returns the full room state.
    #[tracing::instrument(skip(self))]
    pub async fn room_state_full(
        &self,
        room_id: &RoomId,
    ) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>> {
        self.db.room_state_full(room_id).await
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn room_state_get_id(
        &self,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<EventId>>> {
        self.db.room_state_get_id(room_id, event_type, state_key)
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn room_state_get(
        &self,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<PduEvent>>> {
        self.db.room_state_get(room_id, event_type, state_key)
    }

    pub fn get_name(&self, room_id: &RoomId) -> Result<Option<String>> {
        services()
            .rooms
            .state_accessor
            .room_state_get(room_id, &StateEventType::RoomName, "")?
            .map_or(Ok(None), |s| {
                serde_json::from_str(s.content.get())
                    .map(|c: RoomNameEventContent| Some(c.name))
                    .map_err(|e| {
                        error!(
                            "Invalid room name event in database for room {}. {}",
                            room_id, e
                        );
                        Error::bad_database("Invalid room name event in database.")
                    })
            })
    }

    pub fn get_avatar(&self, room_id: &RoomId) -> Result<JsOption<RoomAvatarEventContent>> {
        services()
            .rooms
            .state_accessor
            .room_state_get(room_id, &StateEventType::RoomAvatar, "")?
            .map_or(Ok(JsOption::Undefined), |s| {
                serde_json::from_str(s.content.get())
                    .map_err(|_| Error::bad_database("Invalid room avatar event in database."))
            })
    }

    pub async fn user_can_invite(
        &self,
        room_id: &RoomId,
        sender: &UserId,
        target_user: &UserId,
    ) -> Result<bool> {
        let content = to_raw_value(&RoomMemberEventContent::new(MembershipState::Invite))
            .expect("Event content always serializes");

        let new_event = PduBuilder {
            event_type: ruma::events::TimelineEventType::RoomMember,
            content,
            unsigned: None,
            state_key: Some(target_user.into()),
            redacts: None,
        };

        let mutex_state = Arc::clone(
            services()
                .globals
                .roomid_mutex_state
                .write()
                .await
                .entry(room_id.to_owned())
                .or_default(),
        );
        let state_lock = mutex_state.lock().await;

        Ok(services()
            .rooms
            .timeline
            .create_hash_and_sign_event(new_event, sender, room_id, &state_lock)
            .is_ok())
    }

    pub fn get_member(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
    ) -> Result<Option<RoomMemberEventContent>> {
        services()
            .rooms
            .state_accessor
            .room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())?
            .map_or(Ok(None), |s| {
                serde_json::from_str(s.content.get())
                    .map_err(|_| Error::bad_database("Invalid room member event in database."))
            })
    }
}
