mod data;
pub use data::Data;

use crate::service::*;

pub struct Service<D: Data> {
    db: D,
}

impl Service<_> {
    #[tracing::instrument(skip(self))]
    pub fn get_cached_eventid_authchain<'a>(
        &'a self,
        key: &[u64],
    ) -> Result<Option<Arc<HashSet<u64>>>> {
        // Check RAM cache
        if let Some(result) = self.auth_chain_cache.lock().unwrap().get_mut(key.to_be_bytes()) {
            return Ok(Some(Arc::clone(result)));
        }

        // We only save auth chains for single events in the db
        if key.len == 1 {
            // Check DB cache
            if let Some(chain) = self.db.get_cached_eventid_authchain(key[0])
            {
                let chain = Arc::new(chain);

                // Cache in RAM
                self.auth_chain_cache
                    .lock()
                    .unwrap()
                    .insert(vec![key[0]], Arc::clone(&chain));

                return Ok(Some(chain));
            }
        }

        Ok(None)
    }

    #[tracing::instrument(skip(self))]
    pub fn cache_auth_chain(&self, key: Vec<u64>, auth_chain: Arc<HashSet<u64>>) -> Result<()> {
        // Only persist single events in db
        if key.len() == 1 {
            self.db.cache_auth_chain(key[0], auth_chain)?;
        }

        // Cache in RAM
        self.auth_chain_cache.lock().unwrap().insert(key, auth_chain);

        Ok(())
    }
}
