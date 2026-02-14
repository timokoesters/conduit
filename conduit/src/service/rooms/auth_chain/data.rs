use crate::Result;
use std::{collections::HashSet, sync::Arc};

pub trait Data: Send + Sync {
    fn get_cached_eventid_authchain(
        &self,
        shorteventid: &[u64],
    ) -> Result<Option<Arc<HashSet<u64>>>>;
    fn cache_auth_chain(&self, shorteventid: Vec<u64>, auth_chain: Arc<HashSet<u64>>)
    -> Result<()>;
}
