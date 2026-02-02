use std::{collections::HashSet, sync::Arc};

use super::CompressedStateEvent;
use crate::Result;

pub struct StateDiff {
    pub parent: Option<u64>,
    pub added: Arc<HashSet<CompressedStateEvent>>,
    pub removed: Arc<HashSet<CompressedStateEvent>>,
}

pub trait Data: Send + Sync {
    fn get_statediff(&self, shortstatehash: u64) -> Result<StateDiff>;
    fn save_statediff(&self, shortstatehash: u64, diff: StateDiff) -> Result<()>;
}
