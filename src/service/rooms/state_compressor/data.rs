struct StateDiff {
    parent: Option<u64>,
    added: Vec<CompressedStateEvent>,
    removed: Vec<CompressedStateEvent>,
}

pub trait Data {
    fn get_statediff(shortstatehash: u64) -> Result<StateDiff>;
    fn save_statediff(shortstatehash: u64, diff: StateDiff) -> Result<()>;
}
