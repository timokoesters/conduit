use std::{collections::HashMap, sync::Arc};

use crate::{database::KeyValueDatabase, service, services, utils, Error, PduEvent, Result};
use async_trait::async_trait;
use ruma::{
    events::{
        room::history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
        StateEventType,
    },
    EventId, RoomId, ServerName,
};

#[async_trait]
impl service::rooms::state_accessor::Data for KeyValueDatabase {
    async fn state_full_ids(&self, shortstatehash: u64) -> Result<HashMap<u64, Arc<EventId>>> {
        let full_state = services()
            .rooms
            .state_compressor
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;
        let mut result = HashMap::new();
        let mut i = 0;
        for compressed in full_state.into_iter() {
            let parsed = services()
                .rooms
                .state_compressor
                .parse_compressed_state_event(&compressed)?;
            result.insert(parsed.0, parsed.1);

            i += 1;
            if i % 100 == 0 {
                tokio::task::yield_now().await;
            }
        }
        Ok(result)
    }

    async fn state_full(
        &self,
        shortstatehash: u64,
    ) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>> {
        let full_state = services()
            .rooms
            .state_compressor
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;

        let mut result = HashMap::new();
        let mut i = 0;
        for compressed in full_state {
            let (_, eventid) = services()
                .rooms
                .state_compressor
                .parse_compressed_state_event(&compressed)?;
            if let Some(pdu) = services().rooms.timeline.get_pdu(&eventid)? {
                result.insert(
                    (
                        pdu.kind.to_string().into(),
                        pdu.state_key
                            .as_ref()
                            .ok_or_else(|| Error::bad_database("State event has no state key."))?
                            .clone(),
                    ),
                    pdu,
                );
            }

            i += 1;
            if i % 100 == 0 {
                tokio::task::yield_now().await;
            }
        }

        Ok(result)
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    fn state_get_id(
        &self,
        shortstatehash: u64,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<EventId>>> {
        let shortstatekey = match services()
            .rooms
            .short
            .get_shortstatekey(event_type, state_key)?
        {
            Some(s) => s,
            None => return Ok(None),
        };
        let full_state = services()
            .rooms
            .state_compressor
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;
        Ok(full_state
            .into_iter()
            .find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
            .and_then(|compressed| {
                services()
                    .rooms
                    .state_compressor
                    .parse_compressed_state_event(&compressed)
                    .ok()
                    .map(|(_, id)| id)
            }))
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    fn state_get(
        &self,
        shortstatehash: u64,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<PduEvent>>> {
        self.state_get_id(shortstatehash, event_type, state_key)?
            .map_or(Ok(None), |event_id| {
                services().rooms.timeline.get_pdu(&event_id)
            })
    }

    /// Returns the state hash for this pdu.
    fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_shorteventid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |shorteventid| {
                self.shorteventid_shortstatehash
                    .get(&shorteventid)?
                    .map(|bytes| {
                        utils::u64_from_bytes(&bytes).map_err(|_| {
                            Error::bad_database(
                                "Invalid shortstatehash bytes in shorteventid_shortstatehash",
                            )
                        })
                    })
                    .transpose()
            })
    }

    /// Whether a server is allowed to see an event through federation, based on
    /// the room's history_visibility at that event's state.
    ///
    /// Note: Joined/Invited history visibility not yet implemented.
    #[tracing::instrument(skip(self))]
    fn server_can_see_event(&self, _server_name: &ServerName, event_id: &EventId) -> Result<bool> {
        let shortstatehash = match self.pdu_shortstatehash(event_id) {
            Ok(Some(shortstatehash)) => shortstatehash,
            _ => return Ok(false),
        };

        let history_visibility = self
            .state_get(shortstatehash, &StateEventType::RoomHistoryVisibility, "")?
            .map(|event| serde_json::from_str(event.content.get()))
            .transpose()
            .map_err(|_| Error::bad_database("Invalid room history visibility event in database."))?
            .map(|content: RoomHistoryVisibilityEventContent| content.history_visibility);

        Ok(match history_visibility {
            Some(HistoryVisibility::WorldReadable) => true,
            Some(HistoryVisibility::Shared) => true,
            // TODO: Check if any of the server's users were invited
            // at this point in time.
            Some(HistoryVisibility::Joined) => false,
            Some(HistoryVisibility::Invited) => false,
            _ => false,
        })
    }

    /// Returns the full room state.
    async fn room_state_full(
        &self,
        room_id: &RoomId,
    ) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>> {
        if let Some(current_shortstatehash) =
            services().rooms.state.get_room_shortstatehash(room_id)?
        {
            self.state_full(current_shortstatehash).await
        } else {
            Ok(HashMap::new())
        }
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    fn room_state_get_id(
        &self,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<EventId>>> {
        if let Some(current_shortstatehash) =
            services().rooms.state.get_room_shortstatehash(room_id)?
        {
            self.state_get_id(current_shortstatehash, event_type, state_key)
        } else {
            Ok(None)
        }
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    fn room_state_get(
        &self,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<PduEvent>>> {
        if let Some(current_shortstatehash) =
            services().rooms.state.get_room_shortstatehash(room_id)?
        {
            self.state_get(current_shortstatehash, event_type, state_key)
        } else {
            Ok(None)
        }
    }
}
