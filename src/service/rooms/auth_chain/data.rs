use std::collections::HashSet;
use crate::Result;

pub trait Data {
    fn get_cached_eventid_authchain(&self, shorteventid: u64) -> Result<HashSet<u64>>;
    fn cache_eventid_authchain(&self, shorteventid: u64, auth_chain: &HashSet<u64>) -> Result<()>;
}
