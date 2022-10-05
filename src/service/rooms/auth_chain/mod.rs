mod data;
use std::{sync::Arc, collections::{HashSet, BTreeSet}};

pub use data::Data;
use ruma::{RoomId, EventId, api::client::error::ErrorKind};
use tracing::log::warn;

use crate::{Result, services, Error};

pub struct Service {
    db: Arc<dyn Data>,
}

impl Service {
    #[tracing::instrument(skip(self))]
    pub fn get_cached_eventid_authchain<'a>(
        &'a self,
        key: &[u64],
    ) -> Result<Option<Arc<HashSet<u64>>>> {
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
    ) -> Result<impl Iterator<Item = Arc<EventId>> + 'a> {
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
            if let Some(cached) = services().rooms.auth_chain.get_cached_eventid_authchain(&chunk_key)? {
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
                if let Some(cached) = services().rooms.auth_chain.get_cached_eventid_authchain(&[sevent_id])? {
                    hits2 += 1;
                    chunk_cache.extend(cached.iter().copied());
                } else {
                    misses2 += 1;
                    let auth_chain = Arc::new(self.get_auth_chain_inner(room_id, &event_id)?);
                    services().rooms
                        .auth_chain
                        .cache_auth_chain(vec![sevent_id], Arc::clone(&auth_chain))?;
                    println!(
                        "cache missed event {} with auth chain len {}",
                        event_id,
                        auth_chain.len()
                    );
                    chunk_cache.extend(auth_chain.iter());

                    i += 1;
                    if i % 100 == 0 {
                        tokio::task::yield_now().await;
                    }
                };
            }
            println!(
                "chunk missed with len {}, event hits2: {}, misses2: {}",
                chunk_cache.len(),
                hits2,
                misses2
            );
            let chunk_cache = Arc::new(chunk_cache);
            services().rooms
                .auth_chain.cache_auth_chain(chunk_key, Arc::clone(&chunk_cache))?;
            full_auth_chain.extend(chunk_cache.iter());
        }

        println!(
            "total: {}, chunk hits: {}, misses: {}",
            full_auth_chain.len(),
            hits,
            misses
        );

        Ok(full_auth_chain
            .into_iter()
            .filter_map(move |sid| services().rooms.short.get_eventid_from_short(sid).ok()))
    }

    #[tracing::instrument(skip(self, event_id))]
    fn get_auth_chain_inner(
        &self,
        room_id: &RoomId,
        event_id: &EventId,
    ) -> Result<HashSet<u64>> {
        let mut todo = vec![Arc::from(event_id)];
        let mut found = HashSet::new();

        while let Some(event_id) = todo.pop() {
            match services().rooms.timeline.get_pdu(&event_id) {
                Ok(Some(pdu)) => {
                    if pdu.room_id != room_id {
                        return Err(Error::BadRequest(ErrorKind::Forbidden, "Evil event in db"));
                    }
                    for auth_event in &pdu.auth_events {
                        let sauthevent = services()
                            .rooms.short
                            .get_or_create_shorteventid(auth_event)?;

                        if !found.contains(&sauthevent) {
                            found.insert(sauthevent);
                            todo.push(auth_event.clone());
                        }
                    }
                }
                Ok(None) => {
                    warn!("Could not find pdu mentioned in auth events: {}", event_id);
                }
                Err(e) => {
                    warn!("Could not load event in auth chain: {} {}", event_id, e);
                }
            }
        }

        Ok(found)
    }
}
