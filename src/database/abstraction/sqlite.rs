use std::{future::Future, pin::Pin, sync::Arc, thread};

use crate::{database::Config, Result};

use super::{DatabaseEngine, Tree};

use std::{collections::BTreeMap, sync::RwLock};

use crossbeam::channel::{bounded, Sender as ChannelSender};
use parking_lot::{Mutex, MutexGuard};
use rusqlite::{params, Connection, OptionalExtension};

use tokio::sync::oneshot::Sender;

type SqliteHandle = Arc<Mutex<Connection>>;

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

pub struct SqliteEngine {
    handle: SqliteHandle,
}

impl DatabaseEngine for SqliteEngine {
    fn open(config: &Config) -> Result<Arc<Self>> {
        let conn = Connection::open(format!("{}/conduit.db", &config.database_path))?;

        conn.pragma_update(None, "journal_mode", &"WAL".to_owned())?;

        let handle = Arc::new(Mutex::new(conn));

        Ok(Arc::new(SqliteEngine { handle }))
    }

    fn open_tree(self: &Arc<Self>, name: &str) -> Result<Arc<dyn Tree>> {
        self.handle.lock().execute(format!("CREATE TABLE IF NOT EXISTS {} ( \"key\" BLOB PRIMARY KEY, \"value\" BLOB NOT NULL )", name).as_str(), [])?;

        Ok(Arc::new(SqliteTable {
            engine: Arc::clone(self),
            name: name.to_owned(),
            watchers: RwLock::new(BTreeMap::new()),
        }))
    }
}

pub struct SqliteTable {
    engine: Arc<SqliteEngine>,
    name: String,
    watchers: RwLock<BTreeMap<Vec<u8>, Vec<Sender<()>>>>,
}

type TupleOfBytes = (Vec<u8>, Vec<u8>);

impl SqliteTable {
    fn get_with_guard(
        &self,
        guard: &MutexGuard<'_, Connection>,
        key: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        Ok(guard
            .prepare(format!("SELECT value FROM {} WHERE key = ?", self.name).as_str())?
            .query_row([key], |row| row.get(0))
            .optional()?)
    }

    fn insert_with_guard(
        &self,
        guard: &MutexGuard<'_, Connection>,
        key: &[u8],
        value: &[u8],
    ) -> Result<()> {
        guard.execute(
            format!(
                "INSERT OR REPLACE INTO {} (key, value) VALUES (?, ?)",
                self.name
            )
            .as_str(),
            [key, value],
        )?;
        Ok(())
    }

    fn _iter_from_thread<F>(
        &self,
        mutex: Arc<Mutex<Connection>>,
        f: F,
    ) -> Box<dyn Iterator<Item = TupleOfBytes> + Send>
    where
        F: (FnOnce(MutexGuard<'_, Connection>, ChannelSender<TupleOfBytes>)) + Send + 'static,
    {
        let (s, r) = bounded::<TupleOfBytes>(5);

        thread::spawn(move || {
            let _ = f(mutex.lock(), s);
        });

        Box::new(r.into_iter())
    }
}

macro_rules! iter_from_thread {
    ($self:expr, $sql:expr, $param:expr) => {
        $self._iter_from_thread($self.engine.handle.clone(), move |guard, s| {
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
        self.get_with_guard(&self.engine.handle.lock(), key)
    }

    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        self.insert_with_guard(&self.engine.handle.lock(), key, value)?;

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
        };

        Ok(())
    }

    fn remove(&self, key: &[u8]) -> Result<()> {
        self.engine.handle.lock().execute(
            format!("DELETE FROM {} WHERE key = ?", self.name).as_str(),
            [key],
        )?;
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
        let guard = self.engine.handle.lock();

        let old = self.get_with_guard(&guard, key)?;

        let new =
            crate::utils::increment(old.as_deref()).expect("utils::increment always returns Some");

        self.insert_with_guard(&guard, key, &new)?;

        Ok(new)
    }

    // TODO: make this use take_while

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
        Box::new(self.iter_from(&prefix, false).take_while(move |(key, _)| key.starts_with(&prefix)))
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

    fn clear(&self) -> Result<()> {
        self.engine.handle.lock().execute(
            format!("DELETE FROM {}", self.name).as_str(),
            [],
        )?;
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
