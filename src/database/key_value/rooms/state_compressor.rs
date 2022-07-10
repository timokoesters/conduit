impl service::room::state_compressor::Data for KeyValueDatabase {
    fn get_statediff(shortstatehash: u64) -> Result<StateDiff> {
        let value = self
            .shortstatehash_statediff
            .get(&shortstatehash.to_be_bytes())?
            .ok_or_else(|| Error::bad_database("State hash does not exist"))?;
        let parent =
            utils::u64_from_bytes(&value[0..size_of::<u64>()]).expect("bytes have right length");

        let mut add_mode = true;
        let mut added = HashSet::new();
        let mut removed = HashSet::new();

        let mut i = size_of::<u64>();
        while let Some(v) = value.get(i..i + 2 * size_of::<u64>()) {
            if add_mode && v.starts_with(&0_u64.to_be_bytes()) {
                add_mode = false;
                i += size_of::<u64>();
                continue;
            }
            if add_mode {
                added.insert(v.try_into().expect("we checked the size above"));
            } else {
                removed.insert(v.try_into().expect("we checked the size above"));
            }
            i += 2 * size_of::<u64>();
        }

        StateDiff { parent, added, removed }
    }

    fn save_statediff(shortstatehash: u64, diff: StateDiff) -> Result<()> {
        let mut value = diff.parent.to_be_bytes().to_vec();
        for new in &diff.new {
            value.extend_from_slice(&new[..]);
        }

        if !diff.removed.is_empty() {
            value.extend_from_slice(&0_u64.to_be_bytes());
            for removed in &diff.removed {
                value.extend_from_slice(&removed[..]);
            }
        }

        self.shortstatehash_statediff
            .insert(&shortstatehash.to_be_bytes(), &value)?;
    }
}
