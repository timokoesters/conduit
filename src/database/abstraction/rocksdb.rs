use super::super::Config;
use crate::{utils, Result};

use std::{future::Future, pin::Pin, sync::Arc};

use super::{DatabaseEngine, Tree};

use std::{collections::BTreeMap, sync::RwLock};

pub struct Engine(rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>);

pub struct RocksDbEngineTree<'a> {
    db: Arc<Engine>,
    name: &'a str,
    watchers: RwLock<BTreeMap<Vec<u8>, Vec<tokio::sync::oneshot::Sender<()>>>>,
}

impl DatabaseEngine for Engine {
    fn open(config: &Config) -> Result<Arc<Self>> {
        let mut db_opts = rocksdb::Options::default();
        db_opts.create_if_missing(true);
        db_opts.set_max_open_files(16);
        db_opts.set_compaction_style(rocksdb::DBCompactionStyle::Level);
        db_opts.set_compression_type(rocksdb::DBCompressionType::Snappy);
        db_opts.set_target_file_size_base(256 << 20);
        db_opts.set_write_buffer_size(256 << 20);

        let mut block_based_options = rocksdb::BlockBasedOptions::default();
        block_based_options.set_block_size(512 << 10);
        db_opts.set_block_based_table_factory(&block_based_options);

        let cfs = rocksdb::DBWithThreadMode::<rocksdb::MultiThreaded>::list_cf(
            &db_opts,
            &config.database_path,
        )
        .unwrap_or_default();

        let mut options = rocksdb::Options::default();
        options.set_merge_operator_associative("increment", utils::increment_rocksdb);

        let db = rocksdb::DBWithThreadMode::<rocksdb::MultiThreaded>::open_cf_descriptors(
            &db_opts,
            &config.database_path,
            cfs.iter()
                .map(|name| rocksdb::ColumnFamilyDescriptor::new(name, options.clone())),
        )?;

        Ok(Arc::new(Engine(db)))
    }

    fn open_tree(self: &Arc<Self>, name: &'static str) -> Result<Arc<dyn Tree>> {
        let mut options = rocksdb::Options::default();
        options.set_merge_operator_associative("increment", utils::increment_rocksdb);

        // Create if it doesn't exist
        let _ = self.0.create_cf(name, &options);

        Ok(Arc::new(RocksDbEngineTree {
            name,
            db: Arc::clone(self),
            watchers: RwLock::new(BTreeMap::new()),
        }))
    }
}

impl RocksDbEngineTree<'_> {
    fn cf(&self) -> rocksdb::BoundColumnFamily<'_> {
        self.db.0.cf_handle(self.name).unwrap()
    }
}

impl Tree for RocksDbEngineTree<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.db.0.get_cf(self.cf(), key)?)
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

        Ok(self.db.0.put_cf(self.cf(), key, value)?)
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        Ok(self.db.0.delete_cf(self.cf(), key)?)
    }

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + Send + Sync + 'a> {
        Box::new(
            self.db
                .0
                .iterator_cf(self.cf(), rocksdb::IteratorMode::Start),
        )
    }

    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        Box::new(self.db.0.iterator_cf(
            self.cf(),
            rocksdb::IteratorMode::From(
                from,
                if backwards {
                    rocksdb::Direction::Reverse
                } else {
                    rocksdb::Direction::Forward
                },
            ),
        ))
    }

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>> {
        let stats = rocksdb::perf::get_memory_usage_stats(Some(&[&self.db.0]), None).unwrap();
        dbg!(stats.mem_table_total);
        dbg!(stats.mem_table_unflushed);
        dbg!(stats.mem_table_readers_total);
        dbg!(stats.cache_total);
        // TODO: atomic?
        let old = self.get(key)?;
        let new = utils::increment(old.as_deref()).unwrap();
        self.insert(key, &new)?;
        Ok(new)
    }

    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + Send + 'a> {
        Box::new(
            self.db
                .0
                .iterator_cf(
                    self.cf(),
                    rocksdb::IteratorMode::From(&prefix, rocksdb::Direction::Forward),
                )
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
