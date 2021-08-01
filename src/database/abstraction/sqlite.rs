use super::{DatabaseEngine, Tree};
use crate::{database::Config, Result};
use crossbeam::channel::{
    bounded, unbounded, Receiver as ChannelReceiver, Sender as ChannelSender, TryRecvError,
};
use parking_lot::{Mutex, MutexGuard, RwLock};
use rusqlite::{params, Connection, DatabaseName::Main, OptionalExtension, Params};
use std::{
    collections::HashMap,
    future::Future,
    ops::Deref,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use threadpool::ThreadPool;
use tokio::sync::oneshot::Sender;
use tracing::{debug, warn};

struct Pool {
    writer: Mutex<Connection>,
    readers: Vec<Mutex<Connection>>,
    spills: ConnectionRecycler,
    spill_tracker: Arc<()>,
    path: PathBuf,
}

pub const MILLI: Duration = Duration::from_millis(1);

enum HoldingConn<'a> {
    FromGuard(MutexGuard<'a, Connection>),
    FromRecycled(RecycledConn, Arc<()>),
}

impl<'a> Deref for HoldingConn<'a> {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        match self {
            HoldingConn::FromGuard(guard) => guard.deref(),
            HoldingConn::FromRecycled(conn, _) => conn.deref(),
        }
    }
}

struct ConnectionRecycler(ChannelSender<Connection>, ChannelReceiver<Connection>);

impl ConnectionRecycler {
    fn new() -> Self {
        let (s, r) = unbounded();
        Self(s, r)
    }

    fn recycle(&self, conn: Connection) -> RecycledConn {
        let sender = self.0.clone();

        RecycledConn(Some(conn), sender)
    }

    fn try_take(&self) -> Option<Connection> {
        match self.1.try_recv() {
            Ok(conn) => Some(conn),
            Err(TryRecvError::Empty) => None,
            // as this is pretty impossible, a panic is warranted if it ever occurs
            Err(TryRecvError::Disconnected) => panic!("Receiving channel was disconnected. A a sender is owned by the current struct, this should never happen(!!!)")
        }
    }
}

struct RecycledConn(
    Option<Connection>, // To allow moving out of the struct when `Drop` is called.
    ChannelSender<Connection>,
);

impl Deref for RecycledConn {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.0
            .as_ref()
            .expect("RecycledConn does not have a connection in Option<>")
    }
}

impl Drop for RecycledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.0.take() {
            debug!("Recycled connection");
            if let Err(e) = self.1.send(conn) {
                warn!("Recycling a connection led to the following error: {:?}", e)
            }
        }
    }
}

impl Pool {
    fn new<P: AsRef<Path>>(path: P, num_readers: usize, total_cache_size_mb: f64) -> Result<Self> {
        // calculates cache-size per permanent connection
        // 1. convert MB to KiB
        // 2. divide by permanent connections
        // 3. round down to nearest integer
        let cache_size: u32 = ((total_cache_size_mb * 1024.0) / (num_readers + 1) as f64) as u32;

        let writer = Mutex::new(Self::prepare_conn(&path, Some(cache_size))?);

        let mut readers = Vec::new();

        for _ in 0..num_readers {
            readers.push(Mutex::new(Self::prepare_conn(&path, Some(cache_size))?))
        }

        Ok(Self {
            writer,
            readers,
            spills: ConnectionRecycler::new(),
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

    fn write_lock(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock()
    }

    fn read_lock(&self) -> HoldingConn<'_> {
        // First try to get a connection from the permanent pool
        for r in &self.readers {
            if let Some(reader) = r.try_lock() {
                return HoldingConn::FromGuard(reader);
            }
        }

        debug!("read_lock: All permanent readers locked, obtaining spillover reader...");

        // We didn't get a connection from the permanent pool, so we'll dumpster-dive for recycled connections.
        // Either we have a connection or we dont, if we don't, we make a new one.
        let conn = match self.spills.try_take() {
            Some(conn) => conn,
            None => {
                debug!("read_lock: No recycled connections left, creating new one...");
                Self::prepare_conn(&self.path, None).unwrap()
            }
        };

        // Clone the spill Arc to mark how many spilled connections actually exist.
        let spill_arc = Arc::clone(&self.spill_tracker);

        // Get a sense of how many connections exist now.
        let now_count = Arc::strong_count(&spill_arc) - 1 /* because one is held by the pool */;

        // If the spillover readers are more than the number of total readers, there might be a problem.
        if now_count > self.readers.len() {
            warn!(
                "Database is under high load. Consider increasing sqlite_read_pool_size ({} spillover readers exist)",
                now_count
            );
        }

        // Return the recyclable connection.
        HoldingConn::FromRecycled(self.spills.recycle(conn), spill_arc)
    }
}

pub struct Engine {
    pool: Pool,
    iter_pool: Mutex<ThreadPool>,
}

impl DatabaseEngine for Engine {
    fn open(config: &Config) -> Result<Arc<Self>> {
        let pool = Pool::new(
            Path::new(&config.database_path).join("conduit.db"),
            config.sqlite_read_pool_size,
            config.db_cache_capacity_mb,
        )?;

        pool.write_lock()
            .execute("CREATE TABLE IF NOT EXISTS _noop (\"key\" INT)", params![])?;

        let arc = Arc::new(Engine {
            pool,
            iter_pool: Mutex::new(ThreadPool::new(10)),
        });

        Ok(arc)
    }

    fn open_tree(self: &Arc<Self>, name: &str) -> Result<Arc<dyn Tree>> {
        self.pool.write_lock().execute(format!("CREATE TABLE IF NOT EXISTS {} ( \"key\" BLOB PRIMARY KEY, \"value\" BLOB NOT NULL )", name).as_str(), [])?;

        Ok(Arc::new(SqliteTable {
            engine: Arc::clone(self),
            name: name.to_owned(),
            watchers: RwLock::new(HashMap::new()),
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

    // Reaps (at most) (.len() * `fraction`) (rounded down, min 1) connections.
    pub fn reap_spillover_by_fraction(&self, fraction: f64) {
        let mut reaped = 0;

        let spill_amount = self.pool.spills.1.len() as f64;
        let fraction = fraction.clamp(0.01, 1.0);

        let amount = (spill_amount * fraction).max(1.0) as u32;

        for _ in 0..amount {
            if self.pool.spills.try_take().is_some() {
                reaped += 1;
            }
        }

        debug!("Reaped {} connections", reaped);
    }
}

pub struct SqliteTable {
    engine: Arc<Engine>,
    name: String,
    watchers: RwLock<HashMap<Vec<u8>, Vec<Sender<()>>>>,
}

type TupleOfBytes = (Vec<u8>, Vec<u8>);

impl SqliteTable {
    #[tracing::instrument(skip(self, guard, key))]
    fn get_with_guard(&self, guard: &Connection, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(guard
            .prepare(format!("SELECT value FROM {} WHERE key = ?", self.name).as_str())?
            .query_row([key], |row| row.get(0))
            .optional()?)
    }

    #[tracing::instrument(skip(self, guard, key, value))]
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

    #[tracing::instrument(skip(self, sql, param))]
    fn iter_from_thread(
        &self,
        sql: String,
        param: Option<Vec<u8>>,
    ) -> Box<dyn Iterator<Item = TupleOfBytes> + Send + Sync> {
        let (s, r) = bounded::<TupleOfBytes>(5);

        let engine = Arc::clone(&self.engine);

        let lock = self.engine.iter_pool.lock();
        if lock.active_count() < lock.max_count() {
            lock.execute(move || {
                if let Some(param) = param {
                    iter_from_thread_work(&engine.pool.read_lock(), &s, &sql, [param]);
                } else {
                    iter_from_thread_work(&engine.pool.read_lock(), &s, &sql, []);
                }
            });
        } else {
            std::thread::spawn(move || {
                if let Some(param) = param {
                    iter_from_thread_work(&engine.pool.read_lock(), &s, &sql, [param]);
                } else {
                    iter_from_thread_work(&engine.pool.read_lock(), &s, &sql, []);
                }
            });
        }

        Box::new(r.into_iter())
    }
}

fn iter_from_thread_work<P>(
    guard: &HoldingConn<'_>,
    s: &ChannelSender<(Vec<u8>, Vec<u8>)>,
    sql: &str,
    params: P,
) where
    P: Params,
{
    for bob in guard
        .prepare(sql)
        .unwrap()
        .query_map(params, |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
        .unwrap()
        .map(|r| r.unwrap())
    {
        if s.send(bob).is_err() {
            return;
        }
    }
}

impl Tree for SqliteTable {
    #[tracing::instrument(skip(self, key))]
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_with_guard(&self.engine.pool.read_lock(), key)
    }

    #[tracing::instrument(skip(self, key, value))]
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

    #[tracing::instrument(skip(self, key))]
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

    #[tracing::instrument(skip(self))]
    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = TupleOfBytes> + Send + 'a> {
        let name = self.name.clone();
        self.iter_from_thread(
            format!("SELECT key, value FROM {} ORDER BY key ASC", name),
            None,
        )
    }

    #[tracing::instrument(skip(self, from, backwards))]
    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = TupleOfBytes> + Send + 'a> {
        let name = self.name.clone();
        let from = from.to_vec(); // TODO change interface?
        if backwards {
            self.iter_from_thread(
                format!(
                    "SELECT key, value FROM {} WHERE key <= ? ORDER BY key DESC",
                    name
                ),
                Some(from),
            )
        } else {
            self.iter_from_thread(
                format!(
                    "SELECT key, value FROM {} WHERE key >= ? ORDER BY key ASC",
                    name
                ),
                Some(from),
            )
        }
    }

    #[tracing::instrument(skip(self, key))]
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

    #[tracing::instrument(skip(self, prefix))]
    fn scan_prefix<'a>(
        &'a self,
        prefix: Vec<u8>,
    ) -> Box<dyn Iterator<Item = TupleOfBytes> + Send + 'a> {
        // let name = self.name.clone();
        // self.iter_from_thread(
        //     format!(
        //         "SELECT key, value FROM {} WHERE key BETWEEN ?1 AND ?1 || X'FFFFFFFFFFFFFFFFFFFFFFFFFFFFFFFF' ORDER BY key ASC",
        //         name
        //     )
        //     [prefix]
        // )
        Box::new(
            self.iter_from(&prefix, false)
                .take_while(move |(key, _)| key.starts_with(&prefix)),
        )
    }

    #[tracing::instrument(skip(self, prefix))]
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

    #[tracing::instrument(skip(self))]
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
