mod data;
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

pub use data::Data;
use ruma::{
    api::{
        client::{error::ErrorKind, sync::sync_events::StrippedState},
        federation::membership::RawStrippedState,
    },
    events::{
        room::{create::RoomCreateEventContent, member::MembershipState},
        StateEventType, TimelineEventType, RECOMMENDED_STRIPPED_STATE_EVENT_TYPES,
    },
    room_version_rules::AuthorizationRules,
    serde::Raw,
    state_res::{self, StateMap},
    EventId, OwnedEventId, RoomId, RoomVersionId, UserId,
};
use serde::Deserialize;
use tokio::sync::MutexGuard;
use tracing::warn;

use crate::{services, utils::calculate_hash, Error, PduEvent, Result};

use super::state_compressor::CompressedStateEvent;

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    /// Set the room to the given statehash and update caches.
    pub async fn force_state(
        &self,
        room_id: &RoomId,
        shortstatehash: u64,
        statediffnew: Arc<HashSet<CompressedStateEvent>>,
        _statediffremoved: Arc<HashSet<CompressedStateEvent>>,
        state_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<()> {
        for event_id in statediffnew.iter().filter_map(|new| {
            services()
                .rooms
                .state_compressor
                .parse_compressed_state_event(new)
                .ok()
                .map(|(_, id)| id)
        }) {
            let pdu = match services().rooms.timeline.get_pdu_json(&event_id)? {
                Some(pdu) => pdu,
                None => continue,
            };

            let pdu: PduEvent = match serde_json::from_str(
                &serde_json::to_string(&pdu).expect("CanonicalJsonObj can be serialized to JSON"),
            ) {
                Ok(pdu) => pdu,
                Err(_) => continue,
            };

            match pdu.kind {
                TimelineEventType::RoomMember => {
                    #[derive(Deserialize)]
                    struct ExtractMembership {
                        membership: MembershipState,
                    }

                    let membership =
                        match serde_json::from_str::<ExtractMembership>(pdu.content.get()) {
                            Ok(e) => e.membership,
                            Err(_) => continue,
                        };

                    let state_key = match pdu.state_key {
                        Some(k) => k,
                        None => continue,
                    };

                    let user_id = match UserId::parse(state_key) {
                        Ok(id) => id,
                        Err(_) => continue,
                    };

                    services().rooms.state_cache.update_membership(
                        room_id,
                        &user_id,
                        membership,
                        &pdu.sender,
                        None,
                        false,
                    )?;
                }
                TimelineEventType::SpaceChild => {
                    services()
                        .rooms
                        .spaces
                        .roomid_spacehierarchy_cache
                        .lock()
                        .await
                        .remove(pdu.room_id().as_ref());
                }
                _ => continue,
            }
        }

        services().rooms.state_cache.update_joined_count(room_id)?;

        self.db
            .set_room_state(room_id, shortstatehash, state_lock)?;

        Ok(())
    }

    /// Generates a new StateHash and associates it with the incoming event.
    ///
    /// This adds all current state events (not including the incoming event)
    /// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
    #[tracing::instrument(skip(self, state_ids_compressed))]
    pub fn set_event_state(
        &self,
        event_id: &EventId,
        room_id: &RoomId,
        state_ids_compressed: Arc<HashSet<CompressedStateEvent>>,
    ) -> Result<u64> {
        let shorteventid = services()
            .rooms
            .short
            .get_or_create_shorteventid(event_id)?;

        let previous_shortstatehash = self.db.get_room_shortstatehash(room_id)?;

        let state_hash = calculate_hash(
            &state_ids_compressed
                .iter()
                .map(|s| &s[..])
                .collect::<Vec<_>>(),
        );

        let (shortstatehash, already_existed) = services()
            .rooms
            .short
            .get_or_create_shortstatehash(&state_hash)?;

        if !already_existed {
            let states_parents = previous_shortstatehash.map_or_else(
                || Ok(Vec::new()),
                |p| {
                    services()
                        .rooms
                        .state_compressor
                        .load_shortstatehash_info(p)
                },
            )?;

            let (statediffnew, statediffremoved) =
                if let Some(parent_stateinfo) = states_parents.last() {
                    let statediffnew: HashSet<_> = state_ids_compressed
                        .difference(&parent_stateinfo.1)
                        .copied()
                        .collect();

                    let statediffremoved: HashSet<_> = parent_stateinfo
                        .1
                        .difference(&state_ids_compressed)
                        .copied()
                        .collect();

                    (Arc::new(statediffnew), Arc::new(statediffremoved))
                } else {
                    (state_ids_compressed, Arc::new(HashSet::new()))
                };
            services().rooms.state_compressor.save_state_from_diff(
                shortstatehash,
                statediffnew,
                statediffremoved,
                1_000_000, // high number because no state will be based on this one
                states_parents,
            )?;
        }

        self.db.set_event_state(shorteventid, shortstatehash)?;

        Ok(shortstatehash)
    }

    /// Generates a new StateHash and associates it with the incoming event.
    ///
    /// This adds all current state events (not including the incoming event)
    /// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
    #[tracing::instrument(skip(self, new_pdu))]
    pub fn append_to_state(&self, new_pdu: &PduEvent) -> Result<u64> {
        let shorteventid = services()
            .rooms
            .short
            .get_or_create_shorteventid(&new_pdu.event_id)?;

        let previous_shortstatehash = self.get_room_shortstatehash(&new_pdu.room_id())?;

        if let Some(p) = previous_shortstatehash {
            self.db.set_event_state(shorteventid, p)?;
        }

        if let Some(state_key) = &new_pdu.state_key {
            let states_parents = previous_shortstatehash.map_or_else(
                || Ok(Vec::new()),
                |p| {
                    services()
                        .rooms
                        .state_compressor
                        .load_shortstatehash_info(p)
                },
            )?;

            let shortstatekey = services()
                .rooms
                .short
                .get_or_create_shortstatekey(&new_pdu.kind.to_string().into(), state_key)?;

            let new = services()
                .rooms
                .state_compressor
                .compress_state_event(shortstatekey, &new_pdu.event_id)?;

            let replaces = states_parents
                .last()
                .map(|info| {
                    info.1
                        .iter()
                        .find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
                })
                .unwrap_or_default();

            if Some(&new) == replaces {
                return Ok(previous_shortstatehash.expect("must exist"));
            }

            // TODO: statehash with deterministic inputs
            let shortstatehash = services().globals.next_count()?;

            let mut statediffnew = HashSet::new();
            statediffnew.insert(new);

            let mut statediffremoved = HashSet::new();
            if let Some(replaces) = replaces {
                statediffremoved.insert(*replaces);
            }

            services().rooms.state_compressor.save_state_from_diff(
                shortstatehash,
                Arc::new(statediffnew),
                Arc::new(statediffremoved),
                2,
                states_parents,
            )?;

            Ok(shortstatehash)
        } else {
            Ok(previous_shortstatehash.expect("first event in room must be a state event"))
        }
    }

    #[tracing::instrument(skip(self, room_id))]
    /// Gets all the [recommended stripped state events] from the given room
    ///
    /// [recommended stripped state events]: https://spec.matrix.org/v1.13/client-server-api/#stripped-state
    pub fn stripped_state_federation(&self, room_id: &RoomId) -> Result<Vec<RawStrippedState>> {
        RECOMMENDED_STRIPPED_STATE_EVENT_TYPES
            .iter()
            .filter_map(|state_event_type| {
                services()
                    .rooms
                    .state_accessor
                    .room_state_get(room_id, state_event_type, "")
                    .transpose()
            })
            .map(|e| {
                if e.as_ref()
                    .is_ok_and(|e| e.kind == TimelineEventType::RoomCreate)
                {
                    e.and_then(|e| {
                        services()
                            .rooms
                            .timeline
                            .get_pdu_json(&e.event_id)
                            .transpose()
                            .expect("Event must be present for it to make up the current state")
                            .map(PduEvent::convert_to_outgoing_federation_event)
                            .map(RawStrippedState::Pdu)
                    })
                } else {
                    e.map(|e| RawStrippedState::Stripped(e.to_stripped_state_event()))
                }
            })
            .collect::<Result<Vec<_>>>()
    }

    pub fn stripped_state_client(&self, room_id: &RoomId) -> Result<Vec<Raw<StrippedState>>> {
        RECOMMENDED_STRIPPED_STATE_EVENT_TYPES
            .iter()
            .filter_map(|state_event_type| {
                services()
                    .rooms
                    .state_accessor
                    .room_state_get(room_id, state_event_type, "")
                    .transpose()
            })
            .map(|e| e.map(|e| e.to_stripped_state_event().cast()))
            .collect::<Result<Vec<_>>>()
    }

    /// Set the state hash to a new version, but does not update state_cache.
    #[tracing::instrument(skip(self))]
    pub fn set_room_state(
        &self,
        room_id: &RoomId,
        shortstatehash: u64,
        mutex_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<()> {
        self.db.set_room_state(room_id, shortstatehash, mutex_lock)
    }

    /// Returns the room's version.
    #[tracing::instrument(skip(self))]
    pub fn get_room_version(&self, room_id: &RoomId) -> Result<RoomVersionId> {
        let create_event = services().rooms.state_accessor.room_state_get(
            room_id,
            &StateEventType::RoomCreate,
            "",
        )?;

        let create_event_content: RoomCreateEventContent = create_event
            .as_ref()
            .map(|create_event| {
                serde_json::from_str(create_event.content.get()).map_err(|e| {
                    warn!("Invalid create event: {}", e);
                    Error::bad_database("Invalid create event in db.")
                })
            })
            .transpose()?
            .ok_or_else(|| Error::BadRequest(ErrorKind::InvalidParam, "No create event found"))?;

        Ok(create_event_content.room_version)
    }

    pub fn get_room_shortstatehash(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.db.get_room_shortstatehash(room_id)
    }

    pub fn get_forward_extremities(&self, room_id: &RoomId) -> Result<HashSet<Arc<EventId>>> {
        self.db.get_forward_extremities(room_id)
    }

    pub fn set_forward_extremities(
        &self,
        room_id: &RoomId,
        event_ids: Vec<OwnedEventId>,
        state_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<()> {
        self.db
            .set_forward_extremities(room_id, event_ids, state_lock)
    }

    /// This fetches auth events from the current state.
    #[tracing::instrument(skip(self))]
    pub fn get_auth_events(
        &self,
        room_id: &RoomId,
        kind: &TimelineEventType,
        sender: &UserId,
        state_key: Option<&str>,
        content: &serde_json::value::RawValue,
        auth_rules: &AuthorizationRules,
    ) -> Result<StateMap<Arc<PduEvent>>> {
        let shortstatehash = if let Some(current_shortstatehash) =
            services().rooms.state.get_room_shortstatehash(room_id)?
        {
            current_shortstatehash
        } else {
            return Ok(HashMap::new());
        };

        let mut auth_events =
            state_res::auth_types_for_event(kind, sender, state_key, content, auth_rules)
                .expect("content is a valid JSON object");

        // We always need the room create to check the state anyways, we just need to make sure
        // to remove it when creating events if required to do so by the auth rules.
        if auth_rules.room_create_event_id_as_room_id {
            auth_events.push((StateEventType::RoomCreate, "".into()));
        }

        let mut sauthevents = auth_events
            .into_iter()
            .filter_map(|(event_type, state_key)| {
                services()
                    .rooms
                    .short
                    .get_shortstatekey(&event_type.to_string().into(), &state_key)
                    .ok()
                    .flatten()
                    .map(|s| (s, (event_type, state_key)))
            })
            .collect::<HashMap<_, _>>();

        let full_state = services()
            .rooms
            .state_compressor
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;

        Ok(full_state
            .iter()
            .filter_map(|compressed| {
                services()
                    .rooms
                    .state_compressor
                    .parse_compressed_state_event(compressed)
                    .ok()
            })
            .filter_map(|(shortstatekey, event_id)| {
                sauthevents.remove(&shortstatekey).map(|k| (k, event_id))
            })
            .filter_map(|(k, event_id)| {
                services()
                    .rooms
                    .timeline
                    .get_pdu(&event_id)
                    .ok()
                    .flatten()
                    .map(|pdu| (k, pdu))
            })
            .collect())
    }
}
