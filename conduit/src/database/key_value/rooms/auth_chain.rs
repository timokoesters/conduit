use std::{collections::HashSet, mem::size_of, sync::Arc};

use crate::{Result, database::KeyValueDatabase, service, utils};

impl service::rooms::auth_chain::Data for KeyValueDatabase {
    fn get_cached_eventid_authchain(&self, key: &[u64]) -> Result<Option<Arc<HashSet<u64>>>> {
        // Check RAM cache
        if let Some(result) = self.auth_chain_cache.lock().unwrap().get_mut(key) {
            return Ok(Some(Arc::clone(result)));
        }

        // We only save auth chains for single events in the db
        if key.len() == 1 {
            // Check DB cache
            let chain = self
                .shorteventid_authchain
                .get(&key[0].to_be_bytes())?
                .map(|chain| {
                    chain
                        .chunks_exact(size_of::<u64>())
                        .map(|chunk| utils::u64_from_bytes(chunk).expect("byte length is correct"))
                        .collect()
                });

            if let Some(chain) = chain {
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

    fn cache_auth_chain(&self, key: Vec<u64>, auth_chain: Arc<HashSet<u64>>) -> Result<()> {
        // Only persist single events in db
        if key.len() == 1 {
            self.shorteventid_authchain.insert(
                &key[0].to_be_bytes(),
                &auth_chain
                    .iter()
                    .flat_map(|s| s.to_be_bytes().to_vec())
                    .collect::<Vec<u8>>(),
            )?;
        }

        // Cache in RAM
        self.auth_chain_cache
            .lock()
            .unwrap()
            .insert(key, auth_chain);

        Ok(())
    }
}
