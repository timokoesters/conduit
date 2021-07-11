use std::{
    collections::BTreeMap,
    future::Future,
    ops::Deref,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use crate::{database::Config, Result};

use super::{DatabaseEngine, Tree};

use log::debug;

use crossbeam::channel::{bounded, Sender as ChannelSender};
use parking_lot::{FairMutex, FairMutexGuard, Mutex, MutexGuard, RwLock};
use rusqlite::{params, Connection, DatabaseName::Main, OptionalExtension};

use tokio::sync::oneshot::Sender;

// const SQL_CREATE_TABLE: &str =
//     "CREATE TABLE IF NOT EXISTS {} {{ \"key\" BLOB PRIMARY KEY, \"value\" BLOB NOT NULL }}";
// const SQL_SELECT: &str = "SELECT value FROM {} WHERE key = ?";
// const SQL_INSERT: &str = "INSERT OR REPLACE INTO {} (key, value) VALUES (?, ?)";
// const SQL_DELETE: &str = "DELETE FROM {} WHERE key = ?";
// const SQL_SELECT_ITER: &str = "SELECT key, value FROM {}";
// const SQL_SELECT_PREFIX: &str = "SELECT key, value FROM {} WHERE key LIKE ?||'%' ORDER BY key ASC";
// const SQL_SELECT_ITER_FROM_FORWARDS: &str = "SELECT key, value FROM {} WHERE key >= ? ORDER BY ASC";
// const SQL_SELECT_ITER_FROM_BACKWARDS: &str =
//     "SELECT key, value FROM {} WHERE key <= ? ORDER BY DESC";

struct Pool {
    writer: FairMutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    spill_tracker: Arc<()>,
    path: PathBuf,
}

pub const MILLI: Duration = Duration::from_millis(1);

enum HoldingConn<'a> {
    FromGuard(MutexGuard<'a, Connection>),
    FromOwned(Connection, Arc<()>),
}

impl<'a> Deref for HoldingConn<'a> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        match self {
            HoldingConn::FromGuard(guard) => guard.deref(),
            HoldingConn::FromOwned(conn, _) => conn,
        }
    }
}

impl Pool {
    fn new<P: AsRef<Path>>(path: P, num_readers: usize, cache_size: u32) -> Result<Self> {
        let writer = FairMutex::new(Self::prepare_conn(&path, Some(cache_size))?);

        let mut readers = Vec::new();

        for _ in 0..num_readers {
            readers.push(Mutex::new(Self::prepare_conn(&path, Some(cache_size))?))
        }

        Ok(Self {
            writer,
            readers,
            spill_tracker: Arc::new(()),
            path: path.as_ref().to_path_buf(),
        })
    }

    fn prepare_conn<P: AsRef<Path>>(path: P, cache_size: Option<u32>) -> Result<Connection> {
        let conn = Connection::open(path)?;

        conn.pragma_update(Some(Main), "journal_mode", &"WAL".to_owned())?;

        // conn.pragma_update(Some(Main), "wal_autocheckpoint", &250)?;

        // conn.pragma_update(Some(Main), "wal_checkpoint", &"FULL".to_owned())?;

        conn.pragma_update(Some(Main), "synchronous", &"OFF".to_owned())?;

        if let Some(cache_kib) = cache_size {
            conn.pragma_update(Some(Main), "cache_size", &(-Into::<i64>::into(cache_kib)))?;
        }

        Ok(conn)
    }

    fn write_lock(&self) -> FairMutexGuard<'_, Connection> {
        self.writer.lock()
    }

    fn read_lock(&self) -> HoldingConn<'_> {
        for r in &self.readers {
            if let Some(reader) = r.try_lock() {
                return HoldingConn::FromGuard(reader);
            }
        }

        let spill_arc = self.spill_tracker.clone();
        let now_count = Arc::strong_count(&spill_arc) - 1 /* because one is held by the pool */;

        log::warn!("read_lock: all readers locked, creating spillover reader...");

        if now_count > 1 {
            log::warn!("read_lock: now {} spillover readers exist", now_count);
        }

        let spilled = Self::prepare_conn(&self.path, None).unwrap();

        return HoldingConn::FromOwned(spilled, spill_arc);
    }
}

pub struct Engine {
    pool: Pool,
}

impl DatabaseEngine for Engine {
    fn open(config: &Config) -> Result<Arc<Self>> {
        let pool = Pool::new(
            Path::new(&config.database_path).join("conduit.db"),
            config.sqlite_read_pool_size,
            config.db_cache_capacity / 1024, // bytes -> kb
        )?;

        pool.write_lock()
            .execute("CREATE TABLE IF NOT EXISTS _noop (\"key\" INT)", params![])?;

        let arc = Arc::new(Engine { pool });

        Ok(arc)
    }

    fn open_tree(self: &Arc<Self>, name: &str) -> Result<Arc<dyn Tree>> {
        self.pool.write_lock().execute(format!("CREATE TABLE IF NOT EXISTS {} ( \"key\" BLOB PRIMARY KEY, \"value\" BLOB NOT NULL )", name).as_str(), [])?;

        Ok(Arc::new(SqliteTable {
            engine: Arc::clone(self),
            name: name.to_owned(),
            watchers: RwLock::new(BTreeMap::new()),
        }))
    }

    fn flush(self: &Arc<Self>) -> Result<()> {
        self.pool
            .write_lock()
            .execute_batch(
                "
            PRAGMA synchronous=FULL;
            BEGIN;
                DELETE FROM _noop;
                INSERT INTO _noop VALUES (1);
            COMMIT;
            PRAGMA synchronous=OFF;
            ",
            )
            .map_err(Into::into)
    }
}

impl Engine {
    pub fn flush_wal(self: &Arc<Self>) -> Result<()> {
        self.pool
            .write_lock()
            .execute_batch(
                "
            PRAGMA synchronous=FULL; PRAGMA wal_checkpoint=TRUNCATE;
            BEGIN;
                DELETE FROM _noop;
                INSERT INTO _noop VALUES (1);
            COMMIT;
            PRAGMA wal_checkpoint=PASSIVE; PRAGMA synchronous=OFF;
            ",
            )
            .map_err(Into::into)
    }
}

pub struct SqliteTable {
    engine: Arc<Engine>,
    name: String,
    watchers: RwLock<BTreeMap<Vec<u8>, Vec<Sender<()>>>>,
}

type TupleOfBytes = (Vec<u8>, Vec<u8>);

impl SqliteTable {
    fn get_with_guard(&self, guard: &Connection, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(guard
            .prepare(format!("SELECT value FROM {} WHERE key = ?", self.name).as_str())?
            .query_row([key], |row| row.get(0))
            .optional()?)
    }

    fn insert_with_guard(&self, guard: &Connection, key: &[u8], value: &[u8]) -> Result<()> {
        guard.execute(
            format!(
                "INSERT INTO {} (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                self.name
            )
            .as_str(),
            [key, value],
        )?;
        Ok(())
    }

    fn _iter_from_thread<F>(&self, f: F) -> Box<dyn Iterator<Item = TupleOfBytes> + Send>
    where
        F: (for<'a> FnOnce(&'a Connection, ChannelSender<TupleOfBytes>)) + Send + 'static,
    {
        let (s, r) = bounded::<TupleOfBytes>(5);

        let engine = self.engine.clone();

        thread::spawn(move || {
            let _ = f(&engine.pool.read_lock(), s);
        });

        Box::new(r.into_iter())
    }
}

macro_rules! iter_from_thread {
    ($self:expr, $sql:expr, $param:expr) => {
        $self._iter_from_thread(move |guard, s| {
            let _ = guard
                .prepare($sql)
                .unwrap()
                .query_map($param, |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
                .unwrap()
                .map(|r| r.unwrap())
                .try_for_each(|bob| s.send(bob));
        })
    };
}

impl Tree for SqliteTable {
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let guard = self.engine.pool.read_lock();

        // let start = Instant::now();

        let val = self.get_with_guard(&guard, key);

        // debug!("get:       took {:?}", start.elapsed());
        // debug!("get key: {:?}", &key)

        val
    }

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let guard = self.engine.pool.write_lock();

        let start = Instant::now();

        self.insert_with_guard(&guard, key, value)?;

        let elapsed = start.elapsed();
        if elapsed > MILLI {
            debug!("insert:    took {:012?} : {}", elapsed, &self.name);
        }

        drop(guard);

        let watchers = self.watchers.read();
        let mut triggered = Vec::new();

        for length in 0..=key.len() {
            if watchers.contains_key(&key[..length]) {
                triggered.push(&key[..length]);
            }
        }

        drop(watchers);

        if !triggered.is_empty() {
            let mut watchers = self.watchers.write();
            for prefix in triggered {
                if let Some(txs) = watchers.remove(prefix) {
                    for tx in txs {
                        let _ = tx.send(());
                    }
                }
            }
        };

        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        let guard = self.engine.pool.write_lock();

        let start = Instant::now();

        guard.execute(
            format!("DELETE FROM {} WHERE key = ?", self.name).as_str(),
            [key],
        )?;

        let elapsed = start.elapsed();

        if elapsed > MILLI {
            debug!("remove:    took {:012?} : {}", elapsed, &self.name);
        }
        // debug!("remove key: {:?}", &key);

        Ok(())
    }

    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = TupleOfBytes> + Send + 'a> {
        let name = self.name.clone();
        iter_from_thread!(
            self,
            format!("SELECT key, value FROM {}", name).as_str(),
            params![]
        )
    }

    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = TupleOfBytes> + Send + 'a> {
        let name = self.name.clone();
        let from = from.to_vec(); // TODO change interface?
        if backwards {
            iter_from_thread!(
                self,
                format!( // TODO change to <= on rebase
                    "SELECT key, value FROM {} WHERE key < ? ORDER BY key DESC",
                    name
                )
                .as_str(),
                [from]
            )
        } else {
            iter_from_thread!(
                self,
                format!(
                    "SELECT key, value FROM {} WHERE key >= ? ORDER BY key ASC",
                    name
                )
                .as_str(),
                [from]
            )
        }
    }

    fn increment(&self, key: &[u8]) -> Result<Vec<u8>> {
        let guard = self.engine.pool.write_lock();

        let start = Instant::now();

        let old = self.get_with_guard(&guard, key)?;

        let new =
            crate::utils::increment(old.as_deref()).expect("utils::increment always returns Some");

        self.insert_with_guard(&guard, key, &new)?;

        let elapsed = start.elapsed();

        if elapsed > MILLI {
            debug!("increment: took {:012?} : {}", elapsed, &self.name);
        }
        // debug!("increment key: {:?}", &key);

        Ok(new)
    }

    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = TupleOfBytes> + Send + 'a> {
        // let name = self.name.clone();
        // iter_from_thread!(
        //     self,
        //     format!(
        //         "SELECT key, value FROM {} WHERE key BETWEEN ?1 AND ?1 || X'FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF' ORDER BY key ASC",
        //         name
        //     )
        //     .as_str(),
        //     [prefix]
        // )
        Box::new(
            self.iter_from(&prefix, false)
                .take_while(move |(key, _)| key.starts_with(&prefix)),
        )
    }

    fn watch_prefix<'a>(&'a self, prefix: &[u8]) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        let (tx, rx) = tokio::sync::oneshot::channel();

        self.watchers
            .write()
            .entry(prefix.to_vec())
            .or_default()
            .push(tx);

        Box::pin(async move {
            // Tx is never destroyed
            rx.await.unwrap();
        })
    }

    fn clear(&self) -> Result<()> {
        debug!("clear: running");
        self.engine
            .pool
            .write_lock()
            .execute(format!("DELETE FROM {}", self.name).as_str(), [])?;
        debug!("clear: ran");
        Ok(())
    }
}

// TODO
// struct Pool<const NUM_READERS: usize> {
//     writer: Mutex<Connection>,
//     readers: [Mutex<Connection>; NUM_READERS],
// }

// // then, to pick a reader:
// for r in &pool.readers {
//     if let Ok(reader) = r.try_lock() {
//         // use reader
//     }
// }
// // none unlocked, pick the next reader
// pool.readers[pool.counter.fetch_add(1, Relaxed) % NUM_READERS].lock()
