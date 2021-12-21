use super::super::Config;
use crate::{utils, Result};

use std::{future::Future, pin::Pin, sync::Arc};

use super::{DatabaseEngine, Tree};

use std::{collections::HashMap, sync::RwLock};

pub struct Engine {
    rocks: rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>,
    old_cfs: Vec<String>,
}

pub struct RocksDbEngineTree<'a> {
    db: Arc<Engine>,
    name: &'a str,
    watchers: RwLock<HashMap<Vec<u8>, Vec<tokio::sync::oneshot::Sender<()>>>>,
    write_lock: RwLock<()>,
}

impl DatabaseEngine for Engine {
    fn open(config: &Config) -> Result<Arc<Self>> {
        let mut db_opts = rocksdb::Options::default();
        db_opts.create_if_missing(true);
        db_opts.set_max_open_files(512);
        db_opts.set_compaction_style(rocksdb::DBCompactionStyle::Level);
        db_opts.set_compression_type(rocksdb::DBCompressionType::Zstd);
        db_opts.set_target_file_size_base(2 << 22);
        db_opts.set_max_bytes_for_level_base(2 << 24);
        db_opts.set_max_bytes_for_level_multiplier(2.0);
        db_opts.set_num_levels(8);
        db_opts.set_write_buffer_size(2 << 27);

        let rocksdb_cache =
            rocksdb::Cache::new_lru_cache((config.db_cache_capacity_mb * 1024.0 * 1024.0) as usize)
                .unwrap();

        let mut block_based_options = rocksdb::BlockBasedOptions::default();
        block_based_options.set_block_size(2 << 19);
        block_based_options.set_block_cache(&rocksdb_cache);
        db_opts.set_block_based_table_factory(&block_based_options);

        let cfs = rocksdb::DBWithThreadMode::<rocksdb::MultiThreaded>::list_cf(
            &db_opts,
            &config.database_path,
        )
        .unwrap_or_default();

        let db = rocksdb::DBWithThreadMode::<rocksdb::MultiThreaded>::open_cf_descriptors(
            &db_opts,
            &config.database_path,
            cfs.iter().map(|name| {
                let mut options = rocksdb::Options::default();
                let prefix_extractor = rocksdb::SliceTransform::create_fixed_prefix(1);
                options.set_prefix_extractor(prefix_extractor);

                rocksdb::ColumnFamilyDescriptor::new(name, options)
            }),
        )?;

        Ok(Arc::new(Engine {
            rocks: db,
            old_cfs: cfs,
        }))
    }

    fn open_tree(self: &Arc<Self>, name: &'static str) -> Result<Arc<dyn Tree>> {
        if !self.old_cfs.contains(&name.to_owned()) {
            // Create if it didn't exist
            let mut options = rocksdb::Options::default();
            let prefix_extractor = rocksdb::SliceTransform::create_fixed_prefix(1);
            options.set_prefix_extractor(prefix_extractor);

            let _ = self.rocks.create_cf(name, &options);
            println!("created cf");
        }

        Ok(Arc::new(RocksDbEngineTree {
            name,
            db: Arc::clone(self),
            watchers: RwLock::new(HashMap::new()),
            write_lock: RwLock::new(()),
        }))
    }

    fn flush(self: &Arc<Self>) -> Result<()> {
        // TODO?
        Ok(())
    }
}

impl RocksDbEngineTree<'_> {
    fn cf(&self) -> rocksdb::BoundColumnFamily<'_> {
        self.db.rocks.cf_handle(self.name).unwrap()
    }
}

impl Tree for RocksDbEngineTree<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db.rocks.get_cf(self.cf(), key)?)
    }

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let watchers = self.watchers.read().unwrap();
        let mut triggered = Vec::new();

        for length in 0..=key.len() {
            if watchers.contains_key(&key[..length]) {
                triggered.push(&key[..length]);
            }
        }

        drop(watchers);

        if !triggered.is_empty() {
            let mut watchers = self.watchers.write().unwrap();
            for prefix in triggered {
                if let Some(txs) = watchers.remove(prefix) {
                    for tx in txs {
                        let _ = tx.send(());
                    }
                }
            }
        }

        let lock = self.write_lock.read().unwrap();

        let result = self.db.rocks.put_cf(self.cf(), key, value)?;

        drop(lock);

        Ok(result)
    }

    fn insert_batch<'a>(&self, iter: &mut dyn Iterator<Item = (Vec<u8>, Vec<u8>)>) -> Result<()> {
        for (key, value) in iter {
            self.db.rocks.put_cf(self.cf(), key, value)?;
        }

        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        Ok(self.db.rocks.delete_cf(self.cf(), key)?)
    }

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        Box::new(
            self.db
                .rocks
                .iterator_cf(self.cf(), rocksdb::IteratorMode::Start)
                .map(|(k, v)| (Vec::from(k), Vec::from(v))),
        )
    }

    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        Box::new(
            self.db
                .rocks
                .iterator_cf(
                    self.cf(),
                    rocksdb::IteratorMode::From(
                        from,
                        if backwards {
                            rocksdb::Direction::Reverse
                        } else {
                            rocksdb::Direction::Forward
                        },
                    ),
                )
                .map(|(k, v)| (Vec::from(k), Vec::from(v))),
        )
    }

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>> {
        let lock = self.write_lock.write().unwrap();

        let old = self.db.rocks.get_cf(self.cf(), &key)?;
        let new = utils::increment(old.as_deref()).unwrap();
        self.db.rocks.put_cf(self.cf(), key, &new)?;

        drop(lock);
        Ok(new)
    }

    fn increment_batch<'a>(&self, iter: &mut dyn Iterator<Item = Vec<u8>>) -> Result<()> {
        let lock = self.write_lock.write().unwrap();

        for key in iter {
            let old = self.db.rocks.get_cf(self.cf(), &key)?;
            let new = utils::increment(old.as_deref()).unwrap();
            self.db.rocks.put_cf(self.cf(), key, new)?;
        }

        drop(lock);

        Ok(())
    }

    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        Box::new(
            self.db
                .rocks
                .iterator_cf(
                    self.cf(),
                    rocksdb::IteratorMode::From(&prefix, rocksdb::Direction::Forward),
                )
                .map(|(k, v)| (Vec::from(k), Vec::from(v)))
                .take_while(move |(k, _)| k.starts_with(&prefix)),
        )
    }

    fn watch_prefix<'a>(&'a self, prefix: &[u8]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.watchers
            .write()
            .unwrap()
            .entry(prefix.to_vec())
            .or_default()
            .push(tx);

        Box::pin(async move {
            // Tx is never destroyed
            rx.await.unwrap();
        })
    }
}
