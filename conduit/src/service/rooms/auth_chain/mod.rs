mod data;
use std::{
    collections::{BTreeSet, HashSet},
    sync::Arc,
};

pub use data::Data;
use ruma::{EventId, RoomId, api::client::error::ErrorKind, state_res::StateMap};
use tracing::{debug, error, warn};

use crate::{Error, Result, services};

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    pub fn get_cached_eventid_authchain(&self, key: &[u64]) -> Result<Option<Arc<HashSet<u64>>>> {
        self.db.get_cached_eventid_authchain(key)
    }

    #[tracing::instrument(skip(self))]
    pub fn cache_auth_chain(&self, key: Vec<u64>, auth_chain: Arc<HashSet<u64>>) -> Result<()> {
        self.db.cache_auth_chain(key, auth_chain)
    }

    #[tracing::instrument(skip(self, starting_events))]
    pub async fn get_auth_chain<'a>(
        &self,
        room_id: &RoomId,
        starting_events: Vec<Arc<EventId>>,
    ) -> Result<impl Iterator<Item = Arc<EventId>> + 'a + use<'a>> {
        const NUM_BUCKETS: usize = 50;

        let mut buckets = vec![BTreeSet::new(); NUM_BUCKETS];

        let mut i = 0;
        for id in starting_events {
            let short = services().rooms.short.get_or_create_shorteventid(&id)?;
            let bucket_id = (short % NUM_BUCKETS as u64) as usize;
            buckets[bucket_id].insert((short, id.clone()));
            i += 1;
            if i % 100 == 0 {
                tokio::task::yield_now().await;
            }
        }

        let mut full_auth_chain = HashSet::new();

        let mut hits = 0;
        let mut misses = 0;
        for chunk in buckets {
            if chunk.is_empty() {
                continue;
            }

            let chunk_key: Vec<u64> = chunk.iter().map(|(short, _)| short).copied().collect();
            if let Some(cached) = services()
                .rooms
                .auth_chain
                .get_cached_eventid_authchain(&chunk_key)?
            {
                hits += 1;
                full_auth_chain.extend(cached.iter().copied());
                continue;
            }
            misses += 1;

            let mut chunk_cache = HashSet::new();
            let mut hits2 = 0;
            let mut misses2 = 0;
            let mut i = 0;
            for (sevent_id, event_id) in chunk {
                if let Some(cached) = services()
                    .rooms
                    .auth_chain
                    .get_cached_eventid_authchain(&[sevent_id])?
                {
                    hits2 += 1;
                    chunk_cache.extend(cached.iter().copied());
                } else {
                    misses2 += 1;
                    let auth_chain = Arc::new(self.get_auth_chain_inner(room_id, &event_id)?);
                    services()
                        .rooms
                        .auth_chain
                        .cache_auth_chain(vec![sevent_id], Arc::clone(&auth_chain))?;
                    debug!(
                        event_id = ?event_id,
                        chain_length = ?auth_chain.len(),
                        "Cache missed event"
                    );
                    chunk_cache.extend(auth_chain.iter());

                    i += 1;
                    if i % 100 == 0 {
                        tokio::task::yield_now().await;
                    }
                };
            }
            debug!(
                chunk_cache_length = ?chunk_cache.len(),
                hits = ?hits2,
                misses = ?misses2,
                "Chunk missed",
            );
            let chunk_cache = Arc::new(chunk_cache);
            services()
                .rooms
                .auth_chain
                .cache_auth_chain(chunk_key, Arc::clone(&chunk_cache))?;
            full_auth_chain.extend(chunk_cache.iter());
        }

        debug!(
            chain_length = ?full_auth_chain.len(),
            hits = ?hits,
            misses = ?misses,
            "Auth chain stats",
        );

        Ok(full_auth_chain
            .into_iter()
            .filter_map(move |sid| services().rooms.short.get_eventid_from_short(sid).ok()))
    }

    #[tracing::instrument(skip(self, event_id))]
    fn get_auth_chain_inner(&self, room_id: &RoomId, event_id: &EventId) -> Result<HashSet<u64>> {
        let mut todo = vec![Arc::from(event_id)];
        let mut found = HashSet::new();

        while let Some(event_id) = todo.pop() {
            match services().rooms.timeline.get_pdu(&event_id) {
                Ok(Some(pdu)) => {
                    if pdu.room_id().as_ref() != room_id {
                        return Err(Error::BadRequest(
                            ErrorKind::forbidden(),
                            "Evil event in db",
                        ));
                    }
                    for auth_event in &pdu.auth_events {
                        let sauthevent = services()
                            .rooms
                            .short
                            .get_or_create_shorteventid(auth_event)?;

                        if !found.contains(&sauthevent) {
                            found.insert(sauthevent);
                            todo.push(auth_event.clone());
                        }
                    }
                }
                Ok(None) => {
                    warn!(?event_id, "Could not find pdu mentioned in auth events");
                }
                Err(error) => {
                    error!(?event_id, ?error, "Could not load event in auth chain");
                }
            }
        }

        Ok(found)
    }

    #[tracing::instrument(skip(self, conflicted_state_set))]
    /// Fetches the conflicted state subgraph of the given events
    pub fn get_conflicted_state_subgraph(
        &self,
        room_id: &RoomId,
        conflicted_state_set: &StateMap<Vec<Arc<EventId>>>,
    ) -> Result<HashSet<Arc<EventId>>> {
        let conflicted_event_ids: HashSet<_> =
            conflicted_state_set.values().flatten().cloned().collect();
        let mut conflicted_state_subgraph = HashSet::new();

        let mut stack = vec![conflicted_event_ids.iter().cloned().collect::<Vec<_>>()];
        let mut path = Vec::new();

        let mut seen_events = HashSet::new();

        let next_event = |stack: &mut Vec<Vec<_>>, path: &mut Vec<_>| {
            while stack.last().is_some_and(|s| s.is_empty()) {
                stack.pop();
                path.pop();
            }

            stack.last_mut().and_then(|s| s.pop())
        };

        while let Some(event_id) = next_event(&mut stack, &mut path) {
            path.push(event_id.clone());

            if conflicted_state_subgraph.contains(&event_id) {
                // If we reach a conflicted state subgraph path, this path must also be part of
                // the conflicted state subgraph, as we will eventually reach a conflicted event
                // if we follow this path.
                //
                // We check if path > 1 here and below, as we don't consider a single conflicted
                // event to be a path from one conflicted to another.
                if path.len() > 1 {
                    conflicted_state_subgraph.extend(path.iter().cloned());
                }

                // All possible paths from this event must have been traversed in the iteration
                // that caused this event to be added to the conflicted state subgraph in the first
                // place.
                //
                // We pop the path here and below as it won't be removed by `next_event`, due to us
                // never pushing it's auth events to the stack.
                path.pop();
                continue;
            }

            if conflicted_event_ids.contains(&event_id) && path.len() > 1 {
                conflicted_state_subgraph.extend(path.iter().cloned());
            }

            if seen_events.contains(&event_id) {
                // All possible paths from this event must have been traversed in the iteration
                // that caused this event to be added to the conflicted state subgraph in the first
                // place.
                path.pop();
                continue;
            }

            if let Some(pdu) = services().rooms.timeline.get_pdu(&event_id)? {
                if pdu.room_id().as_ref() != room_id {
                    return Err(Error::BadRequest(
                        ErrorKind::forbidden(),
                        "Evil event in db",
                    ));
                }

                stack.push(pdu.auth_events.clone());
            } else {
                warn!(?event_id, "Could not find pdu mentioned in auth events");
                return Err(Error::BadDatabase(
                    "Missing auth event for PDU stored in database",
                ));
            }

            seen_events.insert(event_id);
        }

        Ok(conflicted_state_subgraph)
    }
}
