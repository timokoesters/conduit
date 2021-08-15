use super::Config;
use crate::Result;

use std::{future::Future, pin::Pin, sync::Arc};

#[cfg(feature = "sled")]
pub mod sled;

#[cfg(feature = "sqlite")]
pub mod sqlite;

#[cfg(feature = "heed")]
pub mod heed;

pub trait DatabaseEngine: Sized {
    fn open(config: &Config) -> Result<Arc<Self>>;
    fn open_tree(self: &Arc<Self>, name: &'static str) -> Result<Arc<dyn Tree>>;
    fn flush(self: &Arc<Self>) -> Result<()>;
}

pub trait Tree: Send + Sync {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()>;
    fn insert_batch<'a>(&self, iter: &mut dyn Iterator<Item = (Vec<u8>, Vec<u8>)>) -> Result<()>;

    fn remove(&self, key: &[u8]) -> Result<()>;

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a>;

    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a>;

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>>;
    fn increment_batch<'a>(&self, iter: &mut dyn Iterator<Item = Vec<u8>>) -> Result<()>;

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
