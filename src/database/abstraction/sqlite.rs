use super::{DatabaseEngine, Tree};
use crate::{database::Config, Result};
use parking_lot::{Mutex, MutexGuard, RwLock};
use rusqlite::{Connection, DatabaseName::Main, OptionalExtension};
use std::{
    cell::RefCell,
    collections::HashMap,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    sync::Arc,
};
use tokio::sync::oneshot::Sender;
use tracing::debug;

thread_local! {
    static READ_CONNECTION: RefCell<Option<&'static Connection>> = RefCell::new(None);
    static READ_CONNECTION_ITERATOR: RefCell<Option<&'static Connection>> = RefCell::new(None);
}

struct PreparedStatementIterator<'a> {
    pub iterator: Box<dyn Iterator<Item = TupleOfBytes> + 'a>,
    pub statement_ref: NonAliasingBox<rusqlite::Statement<'a>>,
}

impl Iterator for PreparedStatementIterator<'_> {
    type Item = TupleOfBytes;

    fn next(&mut self) -> Option<Self::Item> {
        self.iterator.next()
    }
}

struct NonAliasingBox<T>(*mut T);
impl<T> Drop for NonAliasingBox<T> {
    fn drop(&mut self) {
        unsafe { Box::from_raw(self.0) };
    }
}

pub struct Engine {
    writer: Mutex<Connection>,

    path: PathBuf,
    cache_size_per_thread: u32,
}

impl Engine {
    fn prepare_conn(path: &Path, cache_size_kb: u32) -> Result<Connection> {
        let conn = Connection::open(&path)?;

        conn.pragma_update(Some(Main), "page_size", &1024)?;
        conn.pragma_update(Some(Main), "journal_mode", &"WAL")?;
        conn.pragma_update(Some(Main), "synchronous", &"NORMAL")?;
        conn.pragma_update(Some(Main), "cache_size", &(-i64::from(cache_size_kb)))?;
        conn.pragma_update(Some(Main), "wal_autocheckpoint", &8000)?;

        Ok(conn)
    }

    fn write_lock(&self) -> MutexGuard<'_, Connection> {
        self.writer.lock()
    }

    fn read_lock(&self) -> &'static Connection {
        READ_CONNECTION.with(|cell| {
            let connection = &mut cell.borrow_mut();

            if (*connection).is_none() {
                let c = Box::leak(Box::new(
                    Self::prepare_conn(&self.path, self.cache_size_per_thread).unwrap(),
                ));
                **connection = Some(c);
            }

            connection.unwrap()
        })
    }

    fn read_lock_iterator(&self) -> &'static Connection {
        READ_CONNECTION_ITERATOR.with(|cell| {
            let connection = &mut cell.borrow_mut();

            if (*connection).is_none() {
                let c = Box::leak(Box::new(
                    Self::prepare_conn(&self.path, self.cache_size_per_thread).unwrap(),
                ));
                **connection = Some(c);
            }

            connection.unwrap()
        })
    }

    pub fn flush_wal(self: &Arc<Self>) -> Result<()> {
        // We use autocheckpoints
        //self.write_lock()
        //.pragma_update(Some(Main), "wal_checkpoint", &"TRUNCATE")?;
        Ok(())
    }
}

impl DatabaseEngine for Engine {
    fn open(config: &Config) -> Result<Arc<Self>> {
        let path = Path::new(&config.database_path).join("conduit.db");

        // calculates cache-size per permanent connection
        // 1. convert MB to KiB
        // 2. divide by permanent connections
        // 3. round down to nearest integer
        let cache_size_per_thread: u32 =
            ((config.db_cache_capacity_mb * 1024.0) / (num_cpus::get().max(1) + 1) as f64) as u32;

        let writer = Mutex::new(Self::prepare_conn(&path, cache_size_per_thread)?);

        let arc = Arc::new(Engine {
            writer,
            path,
            cache_size_per_thread,
        });

        Ok(arc)
    }

    fn open_tree(self: &Arc<Self>, name: &str) -> Result<Arc<dyn Tree>> {
        self.write_lock().execute(&format!("CREATE TABLE IF NOT EXISTS {} ( \"key\" BLOB PRIMARY KEY, \"value\" BLOB NOT NULL )", name), [])?;

        Ok(Arc::new(SqliteTable {
            engine: Arc::clone(self),
            name: name.to_owned(),
            watchers: RwLock::new(HashMap::new()),
        }))
    }

    fn flush(self: &Arc<Self>) -> Result<()> {
        // we enabled PRAGMA synchronous=normal, so this should not be necessary
        Ok(())
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
                "INSERT OR REPLACE INTO {} (key, value) VALUES (?, ?)",
                self.name
            )
            .as_str(),
            [key, value],
        )?;
        Ok(())
    }

    pub fn iter_with_guard<'a>(
        &'a self,
        guard: &'a Connection,
    ) -> Box<dyn Iterator<Item = TupleOfBytes> + 'a> {
        let statement = Box::leak(Box::new(
            guard
                .prepare(&format!(
                    "SELECT key, value FROM {} ORDER BY key ASC",
                    &self.name
                ))
                .unwrap(),
        ));

        let statement_ref = NonAliasingBox(statement);

        let iterator = Box::new(
            statement
                .query_map([], |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
                .unwrap()
                .map(|r| r.unwrap()),
        );

        Box::new(PreparedStatementIterator {
            iterator,
            statement_ref,
        })
    }
}

impl Tree for SqliteTable {
    #[tracing::instrument(skip(self, key))]
    fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get_with_guard(&self.engine.read_lock(), key)
    }

    #[tracing::instrument(skip(self, key, value))]
    fn insert(&self, key: &[u8], value: &[u8]) -> Result<()> {
        let guard = self.engine.write_lock();
        self.insert_with_guard(&guard, key, value)?;
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

    #[tracing::instrument(skip(self, iter))]
    fn insert_batch<'a>(&self, iter: &mut dyn Iterator<Item = (Vec<u8>, Vec<u8>)>) -> Result<()> {
        let guard = self.engine.write_lock();

        guard.execute("BEGIN", [])?;
        for (key, value) in iter {
            self.insert_with_guard(&guard, &key, &value)?;
        }
        guard.execute("COMMIT", [])?;

        drop(guard);

        Ok(())
    }

    #[tracing::instrument(skip(self, iter))]
    fn increment_batch<'a>(&self, iter: &mut dyn Iterator<Item = Vec<u8>>) -> Result<()> {
        let guard = self.engine.write_lock();

        guard.execute("BEGIN", [])?;
        for key in iter {
            let old = self.get_with_guard(&guard, &key)?;
            let new = crate::utils::increment(old.as_deref())
                .expect("utils::increment always returns Some");
            self.insert_with_guard(&guard, &key, &new)?;
        }
        guard.execute("COMMIT", [])?;

        drop(guard);

        Ok(())
    }

    #[tracing::instrument(skip(self, key))]
    fn remove(&self, key: &[u8]) -> Result<()> {
        let guard = self.engine.write_lock();

        guard.execute(
            format!("DELETE FROM {} WHERE key = ?", self.name).as_str(),
            [key],
        )?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    fn iter<'a>(&'a self) -> Box<dyn Iterator<Item = TupleOfBytes> + 'a> {
        let guard = self.engine.read_lock_iterator();

        self.iter_with_guard(&guard)
    }

    #[tracing::instrument(skip(self, from, backwards))]
    fn iter_from<'a>(
        &'a self,
        from: &[u8],
        backwards: bool,
    ) -> Box<dyn Iterator<Item = TupleOfBytes> + 'a> {
        let guard = self.engine.read_lock_iterator();
        let from = from.to_vec(); // TODO change interface?

        if backwards {
            let statement = Box::leak(Box::new(
                guard
                    .prepare(&format!(
                        "SELECT key, value FROM {} WHERE key <= ? ORDER BY key DESC",
                        &self.name
                    ))
                    .unwrap(),
            ));

            let statement_ref = NonAliasingBox(statement);

            let iterator = Box::new(
                statement
                    .query_map([from], |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
                    .unwrap()
                    .map(|r| r.unwrap()),
            );
            Box::new(PreparedStatementIterator {
                iterator,
                statement_ref,
            })
        } else {
            let statement = Box::leak(Box::new(
                guard
                    .prepare(&format!(
                        "SELECT key, value FROM {} WHERE key >= ? ORDER BY key ASC",
                        &self.name
                    ))
                    .unwrap(),
            ));

            let statement_ref = NonAliasingBox(statement);

            let iterator = Box::new(
                statement
                    .query_map([from], |row| Ok((row.get_unwrap(0), row.get_unwrap(1))))
                    .unwrap()
                    .map(|r| r.unwrap()),
            );

            Box::new(PreparedStatementIterator {
                iterator,
                statement_ref,
            })
        }
    }

    #[tracing::instrument(skip(self, key))]
    fn increment(&self, key: &[u8]) -> Result<Vec<u8>> {
        let guard = self.engine.write_lock();

        let old = self.get_with_guard(&guard, key)?;

        let new =
            crate::utils::increment(old.as_deref()).expect("utils::increment always returns Some");

        self.insert_with_guard(&guard, key, &new)?;

        Ok(new)
    }

    #[tracing::instrument(skip(self, prefix))]
    fn scan_prefix<'a>(&'a self, prefix: Vec<u8>) -> Box<dyn Iterator<Item = TupleOfBytes> + 'a> {
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
            .write_lock()
            .execute(format!("DELETE FROM {}", self.name).as_str(), [])?;
        debug!("clear: ran");
        Ok(())
    }
}
