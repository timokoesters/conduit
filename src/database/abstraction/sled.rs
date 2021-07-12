use super::super::Config;
use crate::{utils, Result};
use log::warn;
use std::{future::Future, pin::Pin, sync::Arc};

use super::{DatabaseEngine, Tree};

pub struct Engine(sled::Db);

pub struct SledEngineTree(sled::Tree);

impl DatabaseEngine for Engine {
    fn open(config: &Config) -> Result<Arc<Self>> {
        Ok(Arc::new(Engine(
            sled::Config::default()
                .path(&config.database_path)
                .cache_capacity(config.db_cache_capacity as u64)
                .use_compression(true)
                .open()?,
        )))
    }

    fn open_tree(self: &Arc<Self>, name: &'static str) -> Result<Arc<dyn Tree>> {
        Ok(Arc::new(SledEngineTree(self.0.open_tree(name)?)))
    }

    fn flush(self: &Arc<Self>) -> Result<()> {
        Ok(()) // noop
    }
}

impl Tree for SledEngineTree {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.0.get(key)?.map(|v| v.to_vec()))
    }

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.0.insert(key, value)?;
        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        self.0.remove(key)?;
        Ok(())
    }

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + Send + 'a> {
        Box::new(
            self.0
                .iter()
                .filter_map(|r| {
                    if let Err(e) = &r {
                        warn!("Error: {}", e);
                    }
                    r.ok()
                })
                .map(|(k, v)| (k.to_vec().into(), v.to_vec().into())),
        )
    }

    fn iter_from(
        &self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + Send> {
        let iter = if backwards {
            self.0.range(..from)
        } else {
            self.0.range(from..)
        };

        let iter = iter
            .filter_map(|r| {
                if let Err(e) = &r {
                    warn!("Error: {}", e);
                }
                r.ok()
            })
            .map(|(k, v)| (k.to_vec().into(), v.to_vec().into()));

        if backwards {
            Box::new(iter.rev())
        } else {
            Box::new(iter)
        }
    }

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>> {
        Ok(self
            .0
            .update_and_fetch(key, utils::increment)
            .map(|o| o.expect("increment always sets a value").to_vec())?)
    }

    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + Send + 'a> {
        let iter = self
            .0
            .scan_prefix(prefix)
            .filter_map(|r| {
                if let Err(e) = &r {
                    warn!("Error: {}", e);
                }
                r.ok()
            })
            .map(|(k, v)| (k.to_vec().into(), v.to_vec().into()));

        Box::new(iter)
    }

    fn watch_prefix<'a>(&'a self, prefix: &[u8]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let prefix = prefix.to_vec();
        Box::pin(async move {
            self.0.watch_prefix(prefix).await;
        })
    }
}
