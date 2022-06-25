use super::Config;
use crate::Result;

use std::{future::Future, pin::Pin, sync::Arc};

#[cfg(feature = "sled")]
pub mod sled;

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "heed")]
pub mod heed;

#[cfg(feature = "rocksdb")]
pub mod rocksdb;

#[cfg(feature = "persy")]
pub mod persy;

#[cfg(any(
    feature = "sqlite",
    feature = "rocksdb",
    feature = "heed",
    feature = "persy"
))]
pub mod watchers;

pub trait KeyValueDatabaseEngine: Send + Sync {
    fn open(config: &Config) -> Result<Self>
    where
        Self: Sized;
    fn open_tree(&self, name: &'static str) -> Result<Arc<dyn Tree>>;
    fn flush(&self) -> Result<()>;
    fn cleanup(&self) -> Result<()> {
        Ok(())
    }
    fn memory_usage(&self) -> Result<String> {
        Ok("Current database engine does not support memory usage reporting.".to_owned())
    }
}

pub trait KeyValueTree: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()>;
    fn insert_batch(&self, iter: &mut dyn Iterator<Item = (Vec<u8>, Vec<u8>)>) -> Result<()>;

    fn remove(&self, key: &[u8]) -> Result<()>;

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a>;

    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a>;

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>>;
    fn increment_batch(&self, iter: &mut dyn Iterator<Item = Vec<u8>>) -> Result<()>;

    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a>;

    fn watch_prefix<'a>(&'a self, prefix: &[u8]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    fn clear(&self) -> Result<()> {
        for (key, _) in self.iter() {
            self.remove(&key)?;
        }

        Ok(())
    }
}
