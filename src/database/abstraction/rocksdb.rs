use super::{super::Config, watchers::Watchers, KeyValueDatabaseEngine, KvTree};
use crate::{utils, Result};
use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, RwLock},
};

pub struct Engine {
    rocks: rocksdb::DBWithThreadMode<rocksdb::MultiThreaded>,
    max_open_files: i32,
    cache: rocksdb::Cache,
    old_cfs: Vec<String>,
}

pub struct RocksDbEngineTree<'a> {
    db: Arc<Engine>,
    name: &'a str,
    watchers: Watchers,
    write_lock: RwLock<()>,
}

fn db_options(max_open_files: i32, rocksdb_cache: &rocksdb::Cache) -> rocksdb::Options {
    let mut block_based_options = rocksdb::BlockBasedOptions::default();
    block_based_options.set_block_cache(rocksdb_cache);
    block_based_options.set_bloom_filter(10.0, false);
    block_based_options.set_block_size(4 * 1024);
    block_based_options.set_cache_index_and_filter_blocks(true);
    block_based_options.set_pin_l0_filter_and_index_blocks_in_cache(true);
    block_based_options.set_optimize_filters_for_memory(true);

    let mut db_opts = rocksdb::Options::default();
    db_opts.set_block_based_table_factory(&block_based_options);
    db_opts.create_if_missing(true);
    db_opts.increase_parallelism(num_cpus::get().try_into().unwrap_or(i32::MAX));
    db_opts.set_max_open_files(max_open_files);
    db_opts.set_compression_type(rocksdb::DBCompressionType::Lz4);
    db_opts.set_bottommost_compression_type(rocksdb::DBCompressionType::Zstd);
    db_opts.set_compaction_style(rocksdb::DBCompactionStyle::Level);

    // https://github.com/facebook/rocksdb/wiki/Setup-Options-and-Basic-Tuning
    db_opts.set_level_compaction_dynamic_level_bytes(true);
    db_opts.set_max_background_jobs(6);
    db_opts.set_bytes_per_sync(1_048_576);

    // https://github.com/facebook/rocksdb/issues/849
    db_opts.set_keep_log_file_num(100);

    // https://github.com/facebook/rocksdb/wiki/WAL-Recovery-Modes#ktoleratecorruptedtailrecords
    //
    // Unclean shutdowns of a Matrix homeserver are likely to be fine when
    // recovered in this manner as it's likely any lost information will be
    // restored via federation.
    db_opts.set_wal_recovery_mode(rocksdb::DBRecoveryMode::TolerateCorruptedTailRecords);

    db_opts
}

impl KeyValueDatabaseEngine for Arc<Engine> {
    fn open(config: &Config) -> Result<Self> {
        let cache_capacity_bytes = (config.db_cache_capacity_mb * 1024.0 * 1024.0) as usize;
        let rocksdb_cache = rocksdb::Cache::new_lru_cache(cache_capacity_bytes);

        let db_opts = db_options(config.rocksdb_max_open_files, &rocksdb_cache);

        let cfs = rocksdb::DBWithThreadMode::<rocksdb::MultiThreaded>::list_cf(
            &db_opts,
            &config.database_path,
        )
        .unwrap_or_default();

        let db = rocksdb::DBWithThreadMode::<rocksdb::MultiThreaded>::open_cf_descriptors(
            &db_opts,
            &config.database_path,
            cfs.iter().map(|name| {
                rocksdb::ColumnFamilyDescriptor::new(
                    name,
                    db_options(config.rocksdb_max_open_files, &rocksdb_cache),
                )
            }),
        )?;

        Ok(Arc::new(Engine {
            rocks: db,
            max_open_files: config.rocksdb_max_open_files,
            cache: rocksdb_cache,
            old_cfs: cfs,
        }))
    }

    fn open_tree(&self, name: &'static str) -> Result<Arc<dyn KvTree>> {
        if !self.old_cfs.contains(&name.to_owned()) {
            // Create if it didn't exist
            let _ = self
                .rocks
                .create_cf(name, &db_options(self.max_open_files, &self.cache));
        }

        Ok(Arc::new(RocksDbEngineTree {
            name,
            db: Arc::clone(self),
            watchers: Watchers::default(),
            write_lock: RwLock::new(()),
        }))
    }

    fn flush(&self) -> Result<()> {
        // TODO?
        Ok(())
    }

    fn memory_usage(&self) -> Result<String> {
        let stats =
            rocksdb::perf::get_memory_usage_stats(Some(&[&self.rocks]), Some(&[&self.cache]))?;
        #[allow(clippy::cast_precision_loss)]
        Ok(format!(
            "Approximate memory usage of all the mem-tables: {:.3} MB\n\
             Approximate memory usage of un-flushed mem-tables: {:.3} MB\n\
             Approximate memory usage of all the table readers: {:.3} MB\n\
             Approximate memory usage by cache: {:.3} MB\n\
             Approximate memory usage by cache pinned: {:.3} MB\n\
             ",
            stats.mem_table_total as f64 / 1024.0 / 1024.0,
            stats.mem_table_unflushed as f64 / 1024.0 / 1024.0,
            stats.mem_table_readers_total as f64 / 1024.0 / 1024.0,
            stats.cache_total as f64 / 1024.0 / 1024.0,
            self.cache.get_pinned_usage() as f64 / 1024.0 / 1024.0,
        ))
    }
}

impl RocksDbEngineTree<'_> {
    fn cf(&self) -> Arc<rocksdb::BoundColumnFamily<'_>> {
        self.db.rocks.cf_handle(self.name).unwrap()
    }
}

impl KvTree for RocksDbEngineTree<'_> {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let readoptions = rocksdb::ReadOptions::default();

        Ok(self.db.rocks.get_cf_opt(&self.cf(), key, &readoptions)?)
    }

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let writeoptions = rocksdb::WriteOptions::default();
        let lock = self.write_lock.read().unwrap();
        self.db
            .rocks
            .put_cf_opt(&self.cf(), key, value, &writeoptions)?;
        drop(lock);

        self.watchers.wake(key);

        Ok(())
    }

    fn insert_batch<'a>(&self, iter: &mut dyn Iterator<Item = (Vec<u8>, Vec<u8>)>) -> Result<()> {
        let writeoptions = rocksdb::WriteOptions::default();
        for (key, value) in iter {
            self.db
                .rocks
                .put_cf_opt(&self.cf(), key, value, &writeoptions)?;
        }

        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        let writeoptions = rocksdb::WriteOptions::default();
        Ok(self
            .db
            .rocks
            .delete_cf_opt(&self.cf(), key, &writeoptions)?)
    }

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        let readoptions = rocksdb::ReadOptions::default();

        Box::new(
            self.db
                .rocks
                .iterator_cf_opt(&self.cf(), readoptions, rocksdb::IteratorMode::Start)
                .map(|r| r.unwrap())
                .map(|(k, v)| (Vec::from(k), Vec::from(v))),
        )
    }

    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        let readoptions = rocksdb::ReadOptions::default();

        Box::new(
            self.db
                .rocks
                .iterator_cf_opt(
                    &self.cf(),
                    readoptions,
                    rocksdb::IteratorMode::From(
                        from,
                        if backwards {
                            rocksdb::Direction::Reverse
                        } else {
                            rocksdb::Direction::Forward
                        },
                    ),
                )
                .map(|r| r.unwrap())
                .map(|(k, v)| (Vec::from(k), Vec::from(v))),
        )
    }

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>> {
        let readoptions = rocksdb::ReadOptions::default();
        let writeoptions = rocksdb::WriteOptions::default();

        let lock = self.write_lock.write().unwrap();

        let old = self.db.rocks.get_cf_opt(&self.cf(), key, &readoptions)?;
        let new = utils::increment(old.as_deref());
        self.db
            .rocks
            .put_cf_opt(&self.cf(), key, &new, &writeoptions)?;

        drop(lock);
        Ok(new)
    }

    fn increment_batch<'a>(&self, iter: &mut dyn Iterator<Item = Vec<u8>>) -> Result<()> {
        let readoptions = rocksdb::ReadOptions::default();
        let writeoptions = rocksdb::WriteOptions::default();

        let lock = self.write_lock.write().unwrap();

        for key in iter {
            let old = self.db.rocks.get_cf_opt(&self.cf(), &key, &readoptions)?;
            let new = utils::increment(old.as_deref());
            self.db
                .rocks
                .put_cf_opt(&self.cf(), key, new, &writeoptions)?;
        }

        drop(lock);

        Ok(())
    }

    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = (Vec<u8>, Vec<u8>)> + 'a> {
        let readoptions = rocksdb::ReadOptions::default();

        Box::new(
            self.db
                .rocks
                .iterator_cf_opt(
                    &self.cf(),
                    readoptions,
                    rocksdb::IteratorMode::From(&prefix, rocksdb::Direction::Forward),
                )
                .map(|r| r.unwrap())
                .map(|(k, v)| (Vec::from(k), Vec::from(v)))
                .take_while(move |(k, _)| k.starts_with(&prefix)),
        )
    }

    fn watch_prefix<'a>(&'a self, prefix: &[u8]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        self.watchers.watch(prefix)
    }
}
