use super::CompressedStateEvent;
use crate::Result;

pub struct StateDiff {
    parent: Option<u64>,
    added: Vec<CompressedStateEvent>,
    removed: Vec<CompressedStateEvent>,
}

pub trait Data {
    fn get_statediff(&self, shortstatehash: u64) -> Result<StateDiff>;
    fn save_statediff(&self, shortstatehash: u64, diff: StateDiff) -> Result<()>;
}
