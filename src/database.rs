pub mod abstraction;

pub mod account_data;
pub mod admin;
pub mod appservice;
pub mod globals;
pub mod key_backups;
pub mod media;
pub mod proxy;
pub mod pusher;
pub mod rooms;
pub mod sending;
pub mod transaction_ids;
pub mod uiaa;
pub mod users;

use crate::{utils, Error, Result};
use abstraction::DatabaseEngine;
use directories::ProjectDirs;
use log::error;
use lru_cache::LruCache;
use rocket::{
    futures::{channel::mpsc, stream::FuturesUnordered, StreamExt},
    outcome::IntoOutcome,
    request::{FromRequest, Request},
    try_outcome, State,
};
use ruma::{DeviceId, ServerName, UserId};
use serde::{de::IgnoredAny, Deserialize};
use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, remove_dir_all},
    io::Write,
    ops::Deref,
    sync::{Arc, RwLock},
};
use tokio::sync::{OwnedRwLockReadGuard, RwLock as TokioRwLock, Semaphore};

use self::proxy::ProxyConfig;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    server_name: Box<ServerName>,
    database_path: String,
    #[serde(default = "default_db_cache_capacity")]
    db_cache_capacity: u32,
    #[serde(default = "default_sqlite_read_pool_size")]
    sqlite_read_pool_size: usize,
    #[serde(default = "false_fn")]
    sqlite_wal_clean_timer: bool,
    #[serde(default = "default_sqlite_wal_clean_second_interval")]
    sqlite_wal_clean_second_interval: u32,
    #[serde(default = "default_sqlite_wal_clean_second_timeout")]
    sqlite_wal_clean_second_timeout: u32,
    #[serde(default = "default_max_request_size")]
    max_request_size: u32,
    #[serde(default = "default_max_concurrent_requests")]
    max_concurrent_requests: u16,
    #[serde(default = "true_fn")]
    allow_registration: bool,
    #[serde(default = "true_fn")]
    allow_encryption: bool,
    #[serde(default = "false_fn")]
    allow_federation: bool,
    #[serde(default = "false_fn")]
    pub allow_jaeger: bool,
    #[serde(default)]
    proxy: ProxyConfig,
    jwt_secret: Option<String>,
    #[serde(default = "Vec::new")]
    trusted_servers: Vec<Box<ServerName>>,
    #[serde(default = "default_log")]
    pub log: String,

    #[serde(flatten)]
    catchall: BTreeMap<String, IgnoredAny>,
}

const DEPRECATED_KEYS: &[&str] = &["cache_capacity"];

impl Config {
    pub fn warn_deprecated(&self) {
        let mut was_deprecated = false;
        for key in self
            .catchall
            .keys()
            .filter(|key| DEPRECATED_KEYS.iter().any(|s| s == key))
        {
            log::warn!("Config parameter {} is deprecated", key);
            was_deprecated = true;
        }

        if was_deprecated {
            log::warn!("Read conduit documentation and check your configuration if any new configuration parameters should be adjusted");
        }
    }
}

fn false_fn() -> bool {
    false
}

fn true_fn() -> bool {
    true
}

fn default_db_cache_capacity() -> u32 {
    1024 * 1024 * 1024
}

fn default_sqlite_read_pool_size() -> usize {
    num_cpus::get().max(1)
}

fn default_sqlite_wal_clean_second_interval() -> u32 {
    60
}

fn default_sqlite_wal_clean_second_timeout() -> u32 {
    2
}

fn default_max_request_size() -> u32 {
    20 * 1024 * 1024 // Default to 20 MB
}

fn default_max_concurrent_requests() -> u16 {
    100
}

fn default_log() -> String {
    "info,state_res=warn,rocket=off,_=off,sled=off".to_owned()
}

#[cfg(feature = "sled")]
pub type Engine = abstraction::sled::Engine;

#[cfg(feature = "rocksdb")]
pub type Engine = abstraction::rocksdb::Engine;

#[cfg(feature = "sqlite")]
pub type Engine = abstraction::sqlite::Engine;

pub struct Database {
    _db: Arc<Engine>,
    pub globals: globals::Globals,
    pub users: users::Users,
    pub uiaa: uiaa::Uiaa,
    pub rooms: rooms::Rooms,
    pub account_data: account_data::AccountData,
    pub media: media::Media,
    pub key_backups: key_backups::KeyBackups,
    pub transaction_ids: transaction_ids::TransactionIds,
    pub sending: sending::Sending,
    pub admin: admin::Admin,
    pub appservice: appservice::Appservice,
    pub pusher: pusher::PushData,
}

impl Database {
    /// Tries to remove the old database but ignores all errors.
    pub fn try_remove(server_name: &str) -> Result<()> {
        let mut path = ProjectDirs::from("xyz", "koesters", "conduit")
            .ok_or_else(|| Error::bad_config("The OS didn't return a valid home directory path."))?
            .data_dir()
            .to_path_buf();
        path.push(server_name);
        let _ = remove_dir_all(path);

        Ok(())
    }

    /// Load an existing database or create a new one.
    pub async fn load_or_create(config: Config) -> Result<Arc<TokioRwLock<Self>>> {
        let builder = Engine::open(&config)?;

        if config.max_request_size < 1024 {
            eprintln!("ERROR: Max request size is less than 1KB. Please increase it.");
        }

        let (admin_sender, admin_receiver) = mpsc::unbounded();
        let (sending_sender, sending_receiver) = mpsc::unbounded();

        let db = Arc::new(TokioRwLock::from(Self {
            _db: builder.clone(),
            users: users::Users {
                userid_password: builder.open_tree("userid_password")?,
                userid_displayname: builder.open_tree("userid_displayname")?,
                userid_avatarurl: builder.open_tree("userid_avatarurl")?,
                userdeviceid_token: builder.open_tree("userdeviceid_token")?,
                userdeviceid_metadata: builder.open_tree("userdeviceid_metadata")?,
                userid_devicelistversion: builder.open_tree("userid_devicelistversion")?,
                token_userdeviceid: builder.open_tree("token_userdeviceid")?,
                onetimekeyid_onetimekeys: builder.open_tree("onetimekeyid_onetimekeys")?,
                userid_lastonetimekeyupdate: builder.open_tree("userid_lastonetimekeyupdate")?,
                keychangeid_userid: builder.open_tree("keychangeid_userid")?,
                keyid_key: builder.open_tree("keyid_key")?,
                userid_masterkeyid: builder.open_tree("userid_masterkeyid")?,
                userid_selfsigningkeyid: builder.open_tree("userid_selfsigningkeyid")?,
                userid_usersigningkeyid: builder.open_tree("userid_usersigningkeyid")?,
                todeviceid_events: builder.open_tree("todeviceid_events")?,
            },
            uiaa: uiaa::Uiaa {
                userdevicesessionid_uiaainfo: builder.open_tree("userdevicesessionid_uiaainfo")?,
                userdevicesessionid_uiaarequest: builder
                    .open_tree("userdevicesessionid_uiaarequest")?,
            },
            rooms: rooms::Rooms {
                edus: rooms::RoomEdus {
                    readreceiptid_readreceipt: builder.open_tree("readreceiptid_readreceipt")?,
                    roomuserid_privateread: builder.open_tree("roomuserid_privateread")?, // "Private" read receipt
                    roomuserid_lastprivatereadupdate: builder
                        .open_tree("roomuserid_lastprivatereadupdate")?,
                    typingid_userid: builder.open_tree("typingid_userid")?,
                    roomid_lasttypingupdate: builder.open_tree("roomid_lasttypingupdate")?,
                    presenceid_presence: builder.open_tree("presenceid_presence")?,
                    userid_lastpresenceupdate: builder.open_tree("userid_lastpresenceupdate")?,
                },
                pduid_pdu: builder.open_tree("pduid_pdu")?,
                eventid_pduid: builder.open_tree("eventid_pduid")?,
                roomid_pduleaves: builder.open_tree("roomid_pduleaves")?,

                alias_roomid: builder.open_tree("alias_roomid")?,
                aliasid_alias: builder.open_tree("aliasid_alias")?,
                publicroomids: builder.open_tree("publicroomids")?,

                tokenids: builder.open_tree("tokenids")?,

                roomserverids: builder.open_tree("roomserverids")?,
                serverroomids: builder.open_tree("serverroomids")?,
                userroomid_joined: builder.open_tree("userroomid_joined")?,
                roomuserid_joined: builder.open_tree("roomuserid_joined")?,
                roomuseroncejoinedids: builder.open_tree("roomuseroncejoinedids")?,
                userroomid_invitestate: builder.open_tree("userroomid_invitestate")?,
                roomuserid_invitecount: builder.open_tree("roomuserid_invitecount")?,
                userroomid_leftstate: builder.open_tree("userroomid_leftstate")?,
                roomuserid_leftcount: builder.open_tree("roomuserid_leftcount")?,

                userroomid_notificationcount: builder.open_tree("userroomid_notificationcount")?,
                userroomid_highlightcount: builder.open_tree("userroomid_highlightcount")?,

                statekey_shortstatekey: builder.open_tree("statekey_shortstatekey")?,
                stateid_shorteventid: builder.open_tree("stateid_shorteventid")?,
                eventid_shorteventid: builder.open_tree("eventid_shorteventid")?,
                shorteventid_eventid: builder.open_tree("shorteventid_eventid")?,
                shorteventid_shortstatehash: builder.open_tree("shorteventid_shortstatehash")?,
                roomid_shortstatehash: builder.open_tree("roomid_shortstatehash")?,
                statehash_shortstatehash: builder.open_tree("statehash_shortstatehash")?,

                eventid_outlierpdu: builder.open_tree("eventid_outlierpdu")?,
                prevevent_parent: builder.open_tree("prevevent_parent")?,
                pdu_cache: RwLock::new(LruCache::new(10_000)),
            },
            account_data: account_data::AccountData {
                roomuserdataid_accountdata: builder.open_tree("roomuserdataid_accountdata")?,
            },
            media: media::Media {
                mediaid_file: builder.open_tree("mediaid_file")?,
            },
            key_backups: key_backups::KeyBackups {
                backupid_algorithm: builder.open_tree("backupid_algorithm")?,
                backupid_etag: builder.open_tree("backupid_etag")?,
                backupkeyid_backup: builder.open_tree("backupkeyid_backup")?,
            },
            transaction_ids: transaction_ids::TransactionIds {
                userdevicetxnid_response: builder.open_tree("userdevicetxnid_response")?,
            },
            sending: sending::Sending {
                servername_educount: builder.open_tree("servername_educount")?,
                servernamepduids: builder.open_tree("servernamepduids")?,
                servercurrentevents: builder.open_tree("servercurrentevents")?,
                maximum_requests: Arc::new(Semaphore::new(config.max_concurrent_requests as usize)),
                sender: sending_sender,
            },
            admin: admin::Admin {
                sender: admin_sender,
            },
            appservice: appservice::Appservice {
                cached_registrations: Arc::new(RwLock::new(HashMap::new())),
                id_appserviceregistrations: builder.open_tree("id_appserviceregistrations")?,
            },
            pusher: pusher::PushData {
                senderkey_pusher: builder.open_tree("senderkey_pusher")?,
            },
            globals: globals::Globals::load(
                builder.open_tree("global")?,
                builder.open_tree("server_signingkeys")?,
                config.clone(),
            )?,
        }));

        {
            let db = db.read().await;
            // MIGRATIONS
            // TODO: database versions of new dbs should probably not be 0
            if db.globals.database_version()? < 1 {
                for (roomserverid, _) in db.rooms.roomserverids.iter() {
                    let mut parts = roomserverid.split(|&b| b == 0xff);
                    let room_id = parts.next().expect("split always returns one element");
                    let servername = match parts.next() {
                        Some(s) => s,
                        None => {
                            error!("Migration: Invalid roomserverid in db.");
                            continue;
                        }
                    };
                    let mut serverroomid = servername.to_vec();
                    serverroomid.push(0xff);
                    serverroomid.extend_from_slice(room_id);

                    db.rooms.serverroomids.insert(&serverroomid, &[])?;
                }

                db.globals.bump_database_version(1)?;

                println!("Migration: 0 -> 1 finished");
            }

            if db.globals.database_version()? < 2 {
                // We accidentally inserted hashed versions of "" into the db instead of just ""
                for (userid, password) in db.users.userid_password.iter() {
                    let password = utils::string_from_bytes(&password);

                    let empty_hashed_password = password.map_or(false, |password| {
                        argon2::verify_encoded(&password, b"").unwrap_or(false)
                    });

                    if empty_hashed_password {
                        db.users.userid_password.insert(&userid, b"")?;
                    }
                }

                db.globals.bump_database_version(2)?;

                println!("Migration: 1 -> 2 finished");
            }

            if db.globals.database_version()? < 3 {
                // Move media to filesystem
                for (key, content) in db.media.mediaid_file.iter() {
                    if content.len() == 0 {
                        continue;
                    }

                    let path = db.globals.get_media_file(&key);
                    let mut file = fs::File::create(path)?;
                    file.write_all(&content)?;
                    db.media.mediaid_file.insert(&key, &[])?;
                }

                db.globals.bump_database_version(3)?;

                println!("Migration: 2 -> 3 finished");
            }

            if db.globals.database_version()? < 4 {
                // Add federated users to db as deactivated
                for our_user in db.users.iter() {
                    let our_user = our_user?;
                    if db.users.is_deactivated(&our_user)? {
                        continue;
                    }
                    for room in db.rooms.rooms_joined(&our_user) {
                        for user in db.rooms.room_members(&room?) {
                            let user = user?;
                            if user.server_name() != db.globals.server_name() {
                                println!("Migration: Creating user {}", user);
                                db.users.create(&user, None)?;
                            }
                        }
                    }
                }

                db.globals.bump_database_version(4)?;

                println!("Migration: 3 -> 4 finished");
            }
        }

        let guard = db.read().await;

        // This data is probably outdated
        guard.rooms.edus.presenceid_presence.clear()?;

        guard.admin.start_handler(Arc::clone(&db), admin_receiver);
        guard
            .sending
            .start_handler(Arc::clone(&db), sending_receiver);

        drop(guard);

        #[cfg(feature = "sqlite")]
        Self::start_wal_clean_task(&db, &config).await;

        Ok(db)
    }

    pub async fn watch(&self, user_id: &UserId, device_id: &DeviceId) {
        let userid_bytes = user_id.as_bytes().to_vec();
        let mut userid_prefix = userid_bytes.clone();
        userid_prefix.push(0xff);

        let mut userdeviceid_prefix = userid_prefix.clone();
        userdeviceid_prefix.extend_from_slice(device_id.as_bytes());
        userdeviceid_prefix.push(0xff);

        let mut futures = FuturesUnordered::new();

        // Return when *any* user changed his key
        // TODO: only send for user they share a room with
        futures.push(
            self.users
                .todeviceid_events
                .watch_prefix(&userdeviceid_prefix),
        );

        futures.push(self.rooms.userroomid_joined.watch_prefix(&userid_prefix));
        futures.push(
            self.rooms
                .userroomid_invitestate
                .watch_prefix(&userid_prefix),
        );
        futures.push(self.rooms.userroomid_leftstate.watch_prefix(&userid_prefix));

        // Events for rooms we are in
        for room_id in self.rooms.rooms_joined(user_id).filter_map(|r| r.ok()) {
            let roomid_bytes = room_id.as_bytes().to_vec();
            let mut roomid_prefix = roomid_bytes.clone();
            roomid_prefix.push(0xff);

            // PDUs
            futures.push(self.rooms.pduid_pdu.watch_prefix(&roomid_prefix));

            // EDUs
            futures.push(
                self.rooms
                    .edus
                    .roomid_lasttypingupdate
                    .watch_prefix(&roomid_bytes),
            );

            futures.push(
                self.rooms
                    .edus
                    .readreceiptid_readreceipt
                    .watch_prefix(&roomid_prefix),
            );

            // Key changes
            futures.push(self.users.keychangeid_userid.watch_prefix(&roomid_prefix));

            // Room account data
            let mut roomuser_prefix = roomid_prefix.clone();
            roomuser_prefix.extend_from_slice(&userid_prefix);

            futures.push(
                self.account_data
                    .roomuserdataid_accountdata
                    .watch_prefix(&roomuser_prefix),
            );
        }

        let mut globaluserdata_prefix = vec![0xff];
        globaluserdata_prefix.extend_from_slice(&userid_prefix);

        futures.push(
            self.account_data
                .roomuserdataid_accountdata
                .watch_prefix(&globaluserdata_prefix),
        );

        // More key changes (used when user is not joined to any rooms)
        futures.push(self.users.keychangeid_userid.watch_prefix(&userid_prefix));

        // One time keys
        futures.push(
            self.users
                .userid_lastonetimekeyupdate
                .watch_prefix(&userid_bytes),
        );

        futures.push(Box::pin(self.globals.rotate.watch()));

        // Wait until one of them finds something
        futures.next().await;
    }

    pub async fn flush(&self) -> Result<()> {
        let start = std::time::Instant::now();

        let res = self._db.flush();

        log::debug!("flush: took {:?}", start.elapsed());

        res
    }

    #[cfg(feature = "sqlite")]
    pub fn flush_wal(&self) -> Result<()> {
        self._db.flush_wal()
    }

    #[cfg(feature = "sqlite")]
    pub async fn start_wal_clean_task(lock: &Arc<TokioRwLock<Self>>, config: &Config) {
        use tokio::{
            select,
            signal::unix::{signal, SignalKind},
            time::{interval, timeout},
        };

        use std::{
            sync::Weak,
            time::{Duration, Instant},
        };

        let weak: Weak<TokioRwLock<Database>> = Arc::downgrade(&lock);

        let lock_timeout = Duration::from_secs(config.sqlite_wal_clean_second_timeout as u64);
        let timer_interval = Duration::from_secs(config.sqlite_wal_clean_second_interval as u64);
        let do_timer = config.sqlite_wal_clean_timer;

        tokio::spawn(async move {
            let mut i = interval(timer_interval);
            let mut s = signal(SignalKind::hangup()).unwrap();

            loop {
                select! {
                    _ = i.tick(), if do_timer => {
                        log::info!(target: "wal-trunc", "Timer ticked")
                    }
                    _ = s.recv() => {
                        log::info!(target: "wal-trunc", "Received SIGHUP")
                    }
                };

                if let Some(arc) = Weak::upgrade(&weak) {
                    log::info!(target: "wal-trunc", "Rotating sync helpers...");
                    // This actually creates a very small race condition between firing this and trying to acquire the subsequent write lock.
                    // Though it is not a huge deal if the write lock doesn't "catch", as it'll harmlessly time out.
                    arc.read().await.globals.rotate.fire();

                    log::info!(target: "wal-trunc", "Locking...");
                    let guard = {
                        if let Ok(guard) = timeout(lock_timeout, arc.write()).await {
                            guard
                        } else {
                            log::info!(target: "wal-trunc", "Lock failed in timeout, canceled.");
                            continue;
                        }
                    };
                    log::info!(target: "wal-trunc", "Locked, flushing...");
                    let start = Instant::now();
                    if let Err(e) = guard.flush_wal() {
                        log::error!(target: "wal-trunc", "Errored: {}", e);
                    } else {
                        log::info!(target: "wal-trunc", "Flushed in {:?}", start.elapsed());
                    }
                } else {
                    break;
                }
            }
        });
    }
}

pub struct DatabaseGuard(OwnedRwLockReadGuard<Database>);

impl Deref for DatabaseGuard {
    type Target = OwnedRwLockReadGuard<Database>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[rocket::async_trait]
impl<'r> FromRequest<'r> for DatabaseGuard {
    type Error = ();

    async fn from_request(req: &'r Request<'_>) -> rocket::request::Outcome<Self, ()> {
        let db = try_outcome!(req.guard::<State<'_, Arc<TokioRwLock<Database>>>>().await);

        Ok(DatabaseGuard(Arc::clone(&db).read_owned().await)).or_forward(())
    }
}

impl Into<DatabaseGuard> for OwnedRwLockReadGuard<Database> {
    fn into(self) -> DatabaseGuard {
        DatabaseGuard(self)
    }
}
