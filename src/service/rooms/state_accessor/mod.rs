mod data;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

pub use data::Data;
use lru_cache::LruCache;
use ruma::{
    events::{
        room::history_visibility::HistoryVisibility, room::member::MembershipState, StateEventType,
    },
    EventId, OwnedServerName, OwnedUserId, RoomId, ServerName, UserId,
};
use tracing::warn;

use crate::{PduEvent, Result};

pub struct Service {
    pub db: &'static dyn Data,
    pub server_visibility_cache: Mutex<LruCache<(OwnedServerName, u64), bool>>,
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

    pub fn state_get_content(
        &self,
        shortstatehash: u64,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<serde_json::Value>> {
        self.db
            .state_get_content(shortstatehash, event_type, state_key)
    }

    /// Returns the state hash for this pdu.
    pub fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.db.pdu_shortstatehash(event_id)
    }

    /// Whether a server is allowed to see an event through federation, based on
    /// the room's history_visibility at that event's state.
    #[tracing::instrument(skip(self))]
    pub fn server_can_see_event(
        &self,
        server_name: &ServerName,
        current_server_members: &[OwnedUserId],
        event_id: &EventId,
    ) -> Result<bool> {
        let shortstatehash = match self.pdu_shortstatehash(event_id) {
            Ok(Some(shortstatehash)) => shortstatehash,
            _ => return Ok(false),
        };

        if let Some(visibility) = self
            .server_visibility_cache
            .lock()
            .unwrap()
            .get_mut(&(server_name.to_owned(), shortstatehash))
        {
            return Ok(*visibility);
        }

        let history_visibility = self
            .state_get_content(shortstatehash, &StateEventType::RoomHistoryVisibility, "")?
            .map(|content| match content.get("history_visibility") {
                Some(visibility) => HistoryVisibility::from(visibility.as_str().unwrap_or("")),
                None => HistoryVisibility::Shared,
            });

        let visibility = match history_visibility {
            Some(HistoryVisibility::WorldReadable) => {
                // Allow if event was sent while world readable
                true
            }
            Some(HistoryVisibility::Invited) => {
                // Allow if any member on requesting server was AT LEAST invited, else deny
                current_server_members
                    .into_iter()
                    .any(|member| self.user_was_invited(shortstatehash, &member))
            }
            _ => {
                // Allow if any member on requested server was joined, else deny
                current_server_members
                    .into_iter()
                    .any(|member| self.user_was_joined(shortstatehash, &member))
            }
        };

        self.server_visibility_cache
            .lock()
            .unwrap()
            .insert((server_name.to_owned(), shortstatehash), visibility);

        Ok(visibility)
    }

    /// The user was a joined member at this state (potentially in the past)
    fn user_was_joined(&self, shortstatehash: u64, user_id: &UserId) -> bool {
        self.db
            .user_membership(shortstatehash, user_id)
            .map(|s| s == MembershipState::Join)
            .unwrap_or_default() // Return sensible default, i.e. false
    }

    /// The user was an invited or joined room member at this state (potentially
    /// in the past)
    fn user_was_invited(&self, shortstatehash: u64, user_id: &UserId) -> bool {
        self.db
            .user_membership(shortstatehash, user_id)
            .map(|s| s == MembershipState::Join || s == MembershipState::Invite)
            .unwrap_or_default() // Return sensible default, i.e. false
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
}
