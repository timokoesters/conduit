use crate::{
    database::{
        abstraction::{watchers::Watchers, KeyValueDatabaseEngine, KvTree},
        Config,
    },
    Result,
};
use persy::{ByteVec, OpenOptions, Persy, Transaction, TransactionConfig, ValueMode};

use std::{future::Future, pin::Pin, sync::Arc};

use tracing::warn;

pub struct Engine {
    persy: Persy,
}

impl KeyValueDatabaseEngine for Arc<Engine> {
    fn open(config: &Config) -> Result<Self> {
        let mut cfg = persy::Config::new();
        cfg.change_cache_size((config.db_cache_capacity_mb * 1024.0 * 1024.0) as u64);

        let persy = OpenOptions::new()
            .create(true)
            .config(cfg)
            .open(&format!("{}/db.persy", config.database_path))?;
        Ok(Arc::new(Engine { persy }))
    }

    fn open_tree(&self, name: &'static str) -> Result<Arc<dyn KvTree>> {
        // Create if it doesn't exist
        if !self.persy.exists_index(name)? {
            let mut tx = self.persy.begin()?;
            tx.create_index::<ByteVec, ByteVec>(name, ValueMode::Replace)?;
            tx.prepare()?.commit()?;
        }

        Ok(Arc::new(PersyTree {
            persy: self.persy.clone(),
            name: name.to_owned(),
            watchers: Watchers::default(),
        }))
    }

    fn flush(&self) -> Result<()> {
        Ok(())
    }
}

pub struct PersyTree {
    persy: Persy,
    name: String,
    watchers: Watchers,
}

impl PersyTree {
    fn begin(&self) -> Result<Transaction> {
        Ok(self
            .persy
            .begin_with(TransactionConfig::new().set_background_sync(true))?)
    }
}

impl KvTree for PersyTree {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let result = self
            .persy
            .get::<ByteVec, ByteVec>(&self.name, &ByteVec::from(key))?
            .next()
            .map(|v| (*v).to_owned());
        Ok(result)
    }

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.insert_batch(&mut Some((key.to_owned(), value.to_owned())).into_iter())?;
        self.watchers.wake(key);
        Ok(())
    }

    fn insert_batch<'a>(&self, iter: &mut dyn Iterator<Item = (Vec<u8>, Vec<u8>)>) -> Result<()> {
        let mut tx = self.begin()?;
        for (key, value) in iter {
            tx.put::<ByteVec, ByteVec>(
                &self.name,
                ByteVec::from(key.clone()),
                ByteVec::from(value),
            )?;
        }
        tx.prepare()?.commit()?;
        Ok(())
    }

    fn increment_batch<'a>(&self, iter: &mut dyn Iterator<Item = Vec<u8>>) -> Result<()> {
        let mut tx = self.begin()?;
        for key in iter {
            let old = tx
                .get::<ByteVec, ByteVec>(&self.name, &ByteVec::from(key.clone()))?
                .next()
                .map(|v| (*v).to_owned());
            let new = crate::utils::increment(old.as_deref()).unwrap();
            tx.put::<ByteVec, ByteVec>(&self.name, ByteVec::from(key), ByteVec::from(new))?;
        }
        tx.prepare()?.commit()?;
        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        let mut tx = self.begin()?;
        tx.remove::<ByteVec, ByteVec>(&self.name, ByteVec::from(key), None)?;
        tx.prepare()?.commit()?;
        Ok(())
    }

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        let iter = self.persy.range::<ByteVec, ByteVec, _>(&self.name, ..);
        match iter {
            Ok(iter) => Box::new(iter.filter_map(|(k, v)| {
                v.into_iter()
                    .map(|val| ((*k).to_owned(), (*val).to_owned()))
                    .next()
            })),
            Err(e) => {
                warn!("error iterating {:?}", e);
                Box::new(std::iter::empty())
            }
        }
    }

    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        let range = if backwards {
            self.persy
                .range::<ByteVec, ByteVec, _>(&self.name, ..=ByteVec::from(from))
        } else {
            self.persy
                .range::<ByteVec, ByteVec, _>(&self.name, ByteVec::from(from)..)
        };
        match range {
            Ok(iter) => {
                let map = iter.filter_map(|(k, v)| {
                    v.into_iter()
                        .map(|val| ((*k).to_owned(), (*val).to_owned()))
                        .next()
                });
                if backwards {
                    Box::new(map.rev())
                } else {
                    Box::new(map)
                }
            }
            Err(e) => {
                warn!("error iterating with prefix {:?}", e);
                Box::new(std::iter::empty())
            }
        }
    }

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>> {
        self.increment_batch(&mut Some(key.to_owned()).into_iter())?;
        Ok(self.get(key)?.unwrap())
    }

    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        let range_prefix = ByteVec::from(prefix.clone());
        let range = self
            .persy
            .range::<ByteVec, ByteVec, _>(&self.name, range_prefix..);

        match range {
            Ok(iter) => {
                let owned_prefix = prefix.clone();
                Box::new(
                    iter.take_while(move |(k, _)| (*k).starts_with(&owned_prefix))
                        .filter_map(|(k, v)| {
                            v.into_iter()
                                .map(|val| ((*k).to_owned(), (*val).to_owned()))
                                .next()
                        }),
                )
            }
            Err(e) => {
                warn!("error scanning prefix {:?}", e);
                Box::new(std::iter::empty())
            }
        }
    }

    fn watch_prefix<'a>(&'a self, prefix: &[u8]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.watchers.watch(prefix)
    }
}
