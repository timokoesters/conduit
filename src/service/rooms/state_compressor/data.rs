use std::collections::HashSet;

use super::CompressedStateEvent;
use crate::Result;

pub struct StateDiff {
    pub parent: Option<u64>,
    pub added: HashSet<CompressedStateEvent>,
    pub removed: HashSet<CompressedStateEvent>,
}

pub trait Data: Send + Sync {
    fn get_statediff(&self, shortstatehash: u64) -> Result<StateDiff>;
    fn save_statediff(&self, shortstatehash: u64, diff: StateDiff) -> Result<()>;
}
