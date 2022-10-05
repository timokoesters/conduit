mod data;
use std::{sync::Arc, collections::HashSet};

pub use data::Data;

use crate::Result;

pub struct Service {
    db: Box<dyn Data>,
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
}
