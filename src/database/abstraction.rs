use super::Config;
use crate::Result;

use std::{future::Future, pin::Pin, sync::Arc};

#[cfg(feature = "rocksdb")]
pub mod rocksdb;

#[cfg(feature = "sled")]
pub mod sled;

#[cfg(feature = "sqlite")]
pub mod sqlite;

pub trait DatabaseEngine: Sized {
    fn open(config: &Config) -> Result<Arc<Self>>;
    fn open_tree(self: &Arc<Self>, name: &'static str) -> Result<Arc<dyn Tree>>;
}

pub trait Tree: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()>;

    fn remove(&self, key: &[u8]) -> Result<()>;

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + Send + 'a>;

    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + Send + 'a>;

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>>;

    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + Send + 'a>;

    fn watch_prefix<'a>(&'a self, prefix: &[u8]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

    fn clear(&self) -> Result<()> {
        for (key, _) in self.iter() {
            self.remove(&key)?;
        }

        Ok(())
    }
}
