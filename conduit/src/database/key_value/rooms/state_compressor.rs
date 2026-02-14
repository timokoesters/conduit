use std::{collections::HashSet, mem::size_of, sync::Arc};

use crate::{
    Error, Result,
    database::KeyValueDatabase,
    service::{self, rooms::state_compressor::data::StateDiff},
    utils,
};

impl service::rooms::state_compressor::Data for KeyValueDatabase {
    fn get_statediff(&self, shortstatehash: u64) -> Result<StateDiff> {
        let value = self
            .shortstatehash_statediff
            .get(&shortstatehash.to_be_bytes())?
            .ok_or_else(|| Error::bad_database("State hash does not exist"))?;
        let parent =
            utils::u64_from_bytes(&value[0..size_of::<u64>()]).expect("bytes have right length");
        let parent = if parent != 0 { Some(parent) } else { None };

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

        Ok(StateDiff {
            parent,
            added: Arc::new(added),
            removed: Arc::new(removed),
        })
    }

    fn save_statediff(&self, shortstatehash: u64, diff: StateDiff) -> Result<()> {
        let mut value = diff.parent.unwrap_or(0).to_be_bytes().to_vec();
        for new in diff.added.iter() {
            value.extend_from_slice(&new[..]);
        }

        if !diff.removed.is_empty() {
            value.extend_from_slice(&0_u64.to_be_bytes());
            for removed in diff.removed.iter() {
                value.extend_from_slice(&removed[..]);
            }
        }

        self.shortstatehash_statediff
            .insert(&shortstatehash.to_be_bytes(), &value)
    }
}
