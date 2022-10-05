use std::{collections::{BTreeMap, HashMap}, sync::Arc};

use crate::{database::KeyValueDatabase, service, PduEvent, Error, utils, Result, services};
use async_trait::async_trait;
use ruma::{EventId, events::StateEventType, RoomId};

#[async_trait]
impl service::rooms::state_accessor::Data for Arc<KeyValueDatabase> {
    async fn state_full_ids(&self, shortstatehash: u64) -> Result<BTreeMap<u64, Arc<EventId>>> {
        let full_state = services().rooms.state_compressor
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;
        let mut result = BTreeMap::new();
        let mut i = 0;
        for compressed in full_state.into_iter() {
            let parsed = services().rooms.state_compressor.parse_compressed_state_event(compressed)?;
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
        let full_state = services().rooms.state_compressor
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;

        let mut result = HashMap::new();
        let mut i = 0;
        for compressed in full_state {
            let (_, eventid) = services().rooms.state_compressor.parse_compressed_state_event(compressed)?;
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
        let shortstatekey = match services().rooms.short.get_shortstatekey(event_type, state_key)? {
            Some(s) => s,
            None => return Ok(None),
        };
        let full_state = services().rooms.state_compressor
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;
        Ok(full_state
            .into_iter()
            .find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
            .and_then(|compressed| {
                services().rooms.state_compressor.parse_compressed_state_event(compressed)
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
            .map_or(Ok(None), |event_id| services().rooms.timeline.get_pdu(&event_id))
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

    /// Returns the full room state.
    async fn room_state_full(
        &self,
        room_id: &RoomId,
    ) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>> {
        if let Some(current_shortstatehash) = services().rooms.state.get_room_shortstatehash(room_id)? {
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
        if let Some(current_shortstatehash) = services().rooms.state.get_room_shortstatehash(room_id)? {
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
        if let Some(current_shortstatehash) = services().rooms.state.get_room_shortstatehash(room_id)? {
            self.state_get(current_shortstatehash, event_type, state_key)
        } else {
            Ok(None)
        }
    }
}
