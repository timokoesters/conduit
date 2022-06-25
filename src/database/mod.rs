pub mod abstraction;

pub mod account_data;
pub mod admin;
pub mod appservice;
pub mod globals;
pub mod key_backups;
pub mod media;
pub mod pusher;
pub mod rooms;
pub mod sending;
pub mod transaction_ids;
pub mod uiaa;
pub mod users;

use self::admin::create_admin_room;
use crate::{utils, Config, Error, Result};
use abstraction::KeyValueDatabaseEngine;
use directories::ProjectDirs;
use futures_util::{stream::FuturesUnordered, StreamExt};
use lru_cache::LruCache;
use ruma::{
    events::{
        push_rules::PushRulesEventContent, room::message::RoomMessageEventContent,
        GlobalAccountDataEvent, GlobalAccountDataEventType,
    },
    push::Ruleset,
    DeviceId, EventId, RoomId, UserId,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, remove_dir_all},
    io::Write,
    mem::size_of,
    ops::Deref,
    path::Path,
    sync::{Arc, Mutex, RwLock},
};
use tokio::sync::{mpsc, OwnedRwLockReadGuard, RwLock as TokioRwLock, Semaphore};
use tracing::{debug, error, info, warn};

pub struct KeyValueDatabase {
    _db: Arc<dyn KeyValueDatabaseEngine>,
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

impl KeyValueDatabase {
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

    fn check_db_setup(config: &Config) -> Result<()> {
        let path = Path::new(&config.database_path);

        let sled_exists = path.join("db").exists();
        let sqlite_exists = path.join("conduit.db").exists();
        let rocksdb_exists = path.join("IDENTITY").exists();

        let mut count = 0;

        if sled_exists {
            count += 1;
        }

        if sqlite_exists {
            count += 1;
        }

        if rocksdb_exists {
            count += 1;
        }

        if count > 1 {
            warn!("Multiple databases at database_path detected");
            return Ok(());
        }

        if sled_exists && config.database_backend != "sled" {
            return Err(Error::bad_config(
                "Found sled at database_path, but is not specified in config.",
            ));
        }

        if sqlite_exists && config.database_backend != "sqlite" {
            return Err(Error::bad_config(
                "Found sqlite at database_path, but is not specified in config.",
            ));
        }

        if rocksdb_exists && config.database_backend != "rocksdb" {
            return Err(Error::bad_config(
                "Found rocksdb at database_path, but is not specified in config.",
            ));
        }

        Ok(())
    }

    /// Load an existing database or create a new one.
    pub async fn load_or_create(config: &Config) -> Result<Arc<TokioRwLock<Self>>> {
        Self::check_db_setup(config)?;

        if !Path::new(&config.database_path).exists() {
            std::fs::create_dir_all(&config.database_path)
                .map_err(|_| Error::BadConfig("Database folder doesn't exists and couldn't be created (e.g. due to missing permissions). Please create the database folder yourself."))?;
        }

        let builder: Arc<dyn KeyValueDatabaseEngine> = match &*config.database_backend {
            "sqlite" => {
                #[cfg(not(feature = "sqlite"))]
                return Err(Error::BadConfig("Database backend not found."));
                #[cfg(feature = "sqlite")]
                Arc::new(Arc::<abstraction::sqlite::Engine>::open(config)?)
            }
            "rocksdb" => {
                #[cfg(not(feature = "rocksdb"))]
                return Err(Error::BadConfig("Database backend not found."));
                #[cfg(feature = "rocksdb")]
                Arc::new(Arc::<abstraction::rocksdb::Engine>::open(config)?)
            }
            "persy" => {
                #[cfg(not(feature = "persy"))]
                return Err(Error::BadConfig("Database backend not found."));
                #[cfg(feature = "persy")]
                Arc::new(Arc::<abstraction::persy::Engine>::open(config)?)
            }
            _ => {
                return Err(Error::BadConfig("Database backend not found."));
            }
        };

        if config.max_request_size < 1024 {
            eprintln!("ERROR: Max request size is less than 1KB. Please increase it.");
        }

        let (admin_sender, admin_receiver) = mpsc::unbounded_channel();
        let (sending_sender, sending_receiver) = mpsc::unbounded_channel();

        let db = Arc::new(TokioRwLock::from(Self {
            _db: builder.clone(),
            users: users::Users {
                userid_password: builder.open_tree("userid_password")?,
                userid_displayname: builder.open_tree("userid_displayname")?,
                userid_avatarurl: builder.open_tree("userid_avatarurl")?,
                userid_blurhash: builder.open_tree("userid_blurhash")?,
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
                userfilterid_filter: builder.open_tree("userfilterid_filter")?,
                todeviceid_events: builder.open_tree("todeviceid_events")?,
            },
            uiaa: uiaa::Uiaa {
                userdevicesessionid_uiaainfo: builder.open_tree("userdevicesessionid_uiaainfo")?,
                userdevicesessionid_uiaarequest: RwLock::new(BTreeMap::new()),
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
                roomid_joinedcount: builder.open_tree("roomid_joinedcount")?,
                roomid_invitedcount: builder.open_tree("roomid_invitedcount")?,
                roomuseroncejoinedids: builder.open_tree("roomuseroncejoinedids")?,
                userroomid_invitestate: builder.open_tree("userroomid_invitestate")?,
                roomuserid_invitecount: builder.open_tree("roomuserid_invitecount")?,
                userroomid_leftstate: builder.open_tree("userroomid_leftstate")?,
                roomuserid_leftcount: builder.open_tree("roomuserid_leftcount")?,

                disabledroomids: builder.open_tree("disabledroomids")?,

                lazyloadedids: builder.open_tree("lazyloadedids")?,

                userroomid_notificationcount: builder.open_tree("userroomid_notificationcount")?,
                userroomid_highlightcount: builder.open_tree("userroomid_highlightcount")?,

                statekey_shortstatekey: builder.open_tree("statekey_shortstatekey")?,
                shortstatekey_statekey: builder.open_tree("shortstatekey_statekey")?,

                shorteventid_authchain: builder.open_tree("shorteventid_authchain")?,

                roomid_shortroomid: builder.open_tree("roomid_shortroomid")?,

                shortstatehash_statediff: builder.open_tree("shortstatehash_statediff")?,
                eventid_shorteventid: builder.open_tree("eventid_shorteventid")?,
                shorteventid_eventid: builder.open_tree("shorteventid_eventid")?,
                shorteventid_shortstatehash: builder.open_tree("shorteventid_shortstatehash")?,
                roomid_shortstatehash: builder.open_tree("roomid_shortstatehash")?,
                roomsynctoken_shortstatehash: builder.open_tree("roomsynctoken_shortstatehash")?,
                statehash_shortstatehash: builder.open_tree("statehash_shortstatehash")?,

                eventid_outlierpdu: builder.open_tree("eventid_outlierpdu")?,
                softfailedeventids: builder.open_tree("softfailedeventids")?,

                referencedevents: builder.open_tree("referencedevents")?,
                pdu_cache: Mutex::new(LruCache::new(
                    config
                        .pdu_cache_capacity
                        .try_into()
                        .expect("pdu cache capacity fits into usize"),
                )),
                auth_chain_cache: Mutex::new(LruCache::new(
                    (100_000.0 * config.conduit_cache_capacity_modifier) as usize,
                )),
                shorteventid_cache: Mutex::new(LruCache::new(
                    (100_000.0 * config.conduit_cache_capacity_modifier) as usize,
                )),
                eventidshort_cache: Mutex::new(LruCache::new(
                    (100_000.0 * config.conduit_cache_capacity_modifier) as usize,
                )),
                shortstatekey_cache: Mutex::new(LruCache::new(
                    (100_000.0 * config.conduit_cache_capacity_modifier) as usize,
                )),
                statekeyshort_cache: Mutex::new(LruCache::new(
                    (100_000.0 * config.conduit_cache_capacity_modifier) as usize,
                )),
                our_real_users_cache: RwLock::new(HashMap::new()),
                appservice_in_room_cache: RwLock::new(HashMap::new()),
                lazy_load_waiting: Mutex::new(HashMap::new()),
                stateinfo_cache: Mutex::new(LruCache::new(
                    (100.0 * config.conduit_cache_capacity_modifier) as usize,
                )),
                lasttimelinecount_cache: Mutex::new(HashMap::new()),
            },
            account_data: account_data::AccountData {
                roomuserdataid_accountdata: builder.open_tree("roomuserdataid_accountdata")?,
                roomusertype_roomuserdataid: builder.open_tree("roomusertype_roomuserdataid")?,
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
                servernameevent_data: builder.open_tree("servernameevent_data")?,
                servercurrentevent_data: builder.open_tree("servercurrentevent_data")?,
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

        let guard = db.read().await;

        // Matrix resource ownership is based on the server name; changing it
        // requires recreating the database from scratch.
        if guard.users.count()? > 0 {
            let conduit_user =
                UserId::parse_with_server_name("conduit", guard.globals.server_name())
                    .expect("@conduit:server_name is valid");

            if !guard.users.exists(&conduit_user)? {
                error!(
                    "The {} server user does not exist, and the database is not new.",
                    conduit_user
                );
                return Err(Error::bad_database(
                    "Cannot reuse an existing database after changing the server name, please delete the old one first."
                ));
            }
        }

        // If the database has any data, perform data migrations before starting
        let latest_database_version = 11;

        if guard.users.count()? > 0 {
            let db = &*guard;
            // MIGRATIONS
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

                warn!("Migration: 0 -> 1 finished");
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

                warn!("Migration: 1 -> 2 finished");
            }

            if db.globals.database_version()? < 3 {
                // Move media to filesystem
                for (key, content) in db.media.mediaid_file.iter() {
                    if content.is_empty() {
                        continue;
                    }

                    let path = db.globals.get_media_file(&key);
                    let mut file = fs::File::create(path)?;
                    file.write_all(&content)?;
                    db.media.mediaid_file.insert(&key, &[])?;
                }

                db.globals.bump_database_version(3)?;

                warn!("Migration: 2 -> 3 finished");
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

                warn!("Migration: 3 -> 4 finished");
            }

            if db.globals.database_version()? < 5 {
                // Upgrade user data store
                for (roomuserdataid, _) in db.account_data.roomuserdataid_accountdata.iter() {
                    let mut parts = roomuserdataid.split(|&b| b == 0xff);
                    let room_id = parts.next().unwrap();
                    let user_id = parts.next().unwrap();
                    let event_type = roomuserdataid.rsplit(|&b| b == 0xff).next().unwrap();

                    let mut key = room_id.to_vec();
                    key.push(0xff);
                    key.extend_from_slice(user_id);
                    key.push(0xff);
                    key.extend_from_slice(event_type);

                    db.account_data
                        .roomusertype_roomuserdataid
                        .insert(&key, &roomuserdataid)?;
                }

                db.globals.bump_database_version(5)?;

                warn!("Migration: 4 -> 5 finished");
            }

            if db.globals.database_version()? < 6 {
                // Set room member count
                for (roomid, _) in db.rooms.roomid_shortstatehash.iter() {
                    let string = utils::string_from_bytes(&roomid).unwrap();
                    let room_id = <&RoomId>::try_from(string.as_str()).unwrap();
                    db.rooms.update_joined_count(room_id, &db)?;
                }

                db.globals.bump_database_version(6)?;

                warn!("Migration: 5 -> 6 finished");
            }

            if db.globals.database_version()? < 7 {
                // Upgrade state store
                let mut last_roomstates: HashMap<Box<RoomId>, u64> = HashMap::new();
                let mut current_sstatehash: Option<u64> = None;
                let mut current_room = None;
                let mut current_state = HashSet::new();
                let mut counter = 0;

                let mut handle_state =
                    |current_sstatehash: u64,
                     current_room: &RoomId,
                     current_state: HashSet<_>,
                     last_roomstates: &mut HashMap<_, _>| {
                        counter += 1;
                        println!("counter: {}", counter);
                        let last_roomsstatehash = last_roomstates.get(current_room);

                        let states_parents = last_roomsstatehash.map_or_else(
                            || Ok(Vec::new()),
                            |&last_roomsstatehash| {
                                db.rooms.load_shortstatehash_info(dbg!(last_roomsstatehash))
                            },
                        )?;

                        let (statediffnew, statediffremoved) =
                            if let Some(parent_stateinfo) = states_parents.last() {
                                let statediffnew = current_state
                                    .difference(&parent_stateinfo.1)
                                    .copied()
                                    .collect::<HashSet<_>>();

                                let statediffremoved = parent_stateinfo
                                    .1
                                    .difference(&current_state)
                                    .copied()
                                    .collect::<HashSet<_>>();

                                (statediffnew, statediffremoved)
                            } else {
                                (current_state, HashSet::new())
                            };

                        db.rooms.save_state_from_diff(
                            dbg!(current_sstatehash),
                            statediffnew,
                            statediffremoved,
                            2, // every state change is 2 event changes on average
                            states_parents,
                        )?;

                        /*
                        let mut tmp = db.rooms.load_shortstatehash_info(&current_sstatehash, &db)?;
                        let state = tmp.pop().unwrap();
                        println!(
                            "{}\t{}{:?}: {:?} + {:?} - {:?}",
                            current_room,
                            "  ".repeat(tmp.len()),
                            utils::u64_from_bytes(&current_sstatehash).unwrap(),
                            tmp.last().map(|b| utils::u64_from_bytes(&b.0).unwrap()),
                            state
                                .2
                                .iter()
                                .map(|b| utils::u64_from_bytes(&b[size_of::<u64>()..]).unwrap())
                                .collect::<Vec<_>>(),
                            state
                                .3
                                .iter()
                                .map(|b| utils::u64_from_bytes(&b[size_of::<u64>()..]).unwrap())
                                .collect::<Vec<_>>()
                        );
                        */

                        Ok::<_, Error>(())
                    };

                for (k, seventid) in db._db.open_tree("stateid_shorteventid")?.iter() {
                    let sstatehash = utils::u64_from_bytes(&k[0..size_of::<u64>()])
                        .expect("number of bytes is correct");
                    let sstatekey = k[size_of::<u64>()..].to_vec();
                    if Some(sstatehash) != current_sstatehash {
                        if let Some(current_sstatehash) = current_sstatehash {
                            handle_state(
                                current_sstatehash,
                                current_room.as_deref().unwrap(),
                                current_state,
                                &mut last_roomstates,
                            )?;
                            last_roomstates
                                .insert(current_room.clone().unwrap(), current_sstatehash);
                        }
                        current_state = HashSet::new();
                        current_sstatehash = Some(sstatehash);

                        let event_id = db
                            .rooms
                            .shorteventid_eventid
                            .get(&seventid)
                            .unwrap()
                            .unwrap();
                        let string = utils::string_from_bytes(&event_id).unwrap();
                        let event_id = <&EventId>::try_from(string.as_str()).unwrap();
                        let pdu = db.rooms.get_pdu(event_id).unwrap().unwrap();

                        if Some(&pdu.room_id) != current_room.as_ref() {
                            current_room = Some(pdu.room_id.clone());
                        }
                    }

                    let mut val = sstatekey;
                    val.extend_from_slice(&seventid);
                    current_state.insert(val.try_into().expect("size is correct"));
                }

                if let Some(current_sstatehash) = current_sstatehash {
                    handle_state(
                        current_sstatehash,
                        current_room.as_deref().unwrap(),
                        current_state,
                        &mut last_roomstates,
                    )?;
                }

                db.globals.bump_database_version(7)?;

                warn!("Migration: 6 -> 7 finished");
            }

            if db.globals.database_version()? < 8 {
                // Generate short room ids for all rooms
                for (room_id, _) in db.rooms.roomid_shortstatehash.iter() {
                    let shortroomid = db.globals.next_count()?.to_be_bytes();
                    db.rooms.roomid_shortroomid.insert(&room_id, &shortroomid)?;
                    info!("Migration: 8");
                }
                // Update pduids db layout
                let mut batch = db.rooms.pduid_pdu.iter().filter_map(|(key, v)| {
                    if !key.starts_with(b"!") {
                        return None;
                    }
                    let mut parts = key.splitn(2, |&b| b == 0xff);
                    let room_id = parts.next().unwrap();
                    let count = parts.next().unwrap();

                    let short_room_id = db
                        .rooms
                        .roomid_shortroomid
                        .get(room_id)
                        .unwrap()
                        .expect("shortroomid should exist");

                    let mut new_key = short_room_id;
                    new_key.extend_from_slice(count);

                    Some((new_key, v))
                });

                db.rooms.pduid_pdu.insert_batch(&mut batch)?;

                let mut batch2 = db.rooms.eventid_pduid.iter().filter_map(|(k, value)| {
                    if !value.starts_with(b"!") {
                        return None;
                    }
                    let mut parts = value.splitn(2, |&b| b == 0xff);
                    let room_id = parts.next().unwrap();
                    let count = parts.next().unwrap();

                    let short_room_id = db
                        .rooms
                        .roomid_shortroomid
                        .get(room_id)
                        .unwrap()
                        .expect("shortroomid should exist");

                    let mut new_value = short_room_id;
                    new_value.extend_from_slice(count);

                    Some((k, new_value))
                });

                db.rooms.eventid_pduid.insert_batch(&mut batch2)?;

                db.globals.bump_database_version(8)?;

                warn!("Migration: 7 -> 8 finished");
            }

            if db.globals.database_version()? < 9 {
                // Update tokenids db layout
                let mut iter = db
                    .rooms
                    .tokenids
                    .iter()
                    .filter_map(|(key, _)| {
                        if !key.starts_with(b"!") {
                            return None;
                        }
                        let mut parts = key.splitn(4, |&b| b == 0xff);
                        let room_id = parts.next().unwrap();
                        let word = parts.next().unwrap();
                        let _pdu_id_room = parts.next().unwrap();
                        let pdu_id_count = parts.next().unwrap();

                        let short_room_id = db
                            .rooms
                            .roomid_shortroomid
                            .get(room_id)
                            .unwrap()
                            .expect("shortroomid should exist");
                        let mut new_key = short_room_id;
                        new_key.extend_from_slice(word);
                        new_key.push(0xff);
                        new_key.extend_from_slice(pdu_id_count);
                        println!("old {:?}", key);
                        println!("new {:?}", new_key);
                        Some((new_key, Vec::new()))
                    })
                    .peekable();

                while iter.peek().is_some() {
                    db.rooms
                        .tokenids
                        .insert_batch(&mut iter.by_ref().take(1000))?;
                    println!("smaller batch done");
                }

                info!("Deleting starts");

                let batch2: Vec<_> = db
                    .rooms
                    .tokenids
                    .iter()
                    .filter_map(|(key, _)| {
                        if key.starts_with(b"!") {
                            println!("del {:?}", key);
                            Some(key)
                        } else {
                            None
                        }
                    })
                    .collect();

                for key in batch2 {
                    println!("del");
                    db.rooms.tokenids.remove(&key)?;
                }

                db.globals.bump_database_version(9)?;

                warn!("Migration: 8 -> 9 finished");
            }

            if db.globals.database_version()? < 10 {
                // Add other direction for shortstatekeys
                for (statekey, shortstatekey) in db.rooms.statekey_shortstatekey.iter() {
                    db.rooms
                        .shortstatekey_statekey
                        .insert(&shortstatekey, &statekey)?;
                }

                // Force E2EE device list updates so we can send them over federation
                for user_id in db.users.iter().filter_map(|r| r.ok()) {
                    db.users
                        .mark_device_key_update(&user_id, &db.rooms, &db.globals)?;
                }

                db.globals.bump_database_version(10)?;

                warn!("Migration: 9 -> 10 finished");
            }

            if db.globals.database_version()? < 11 {
                db._db
                    .open_tree("userdevicesessionid_uiaarequest")?
                    .clear()?;
                db.globals.bump_database_version(11)?;

                warn!("Migration: 10 -> 11 finished");
            }

            assert_eq!(11, latest_database_version);

            info!(
                "Loaded {} database with version {}",
                config.database_backend, latest_database_version
            );
        } else {
            guard
                .globals
                .bump_database_version(latest_database_version)?;

            // Create the admin room and server user on first run
            create_admin_room(&guard).await?;

            warn!(
                "Created new {} database with version {}",
                config.database_backend, latest_database_version
            );
        }

        // This data is probably outdated
        guard.rooms.edus.presenceid_presence.clear()?;

        guard.admin.start_handler(Arc::clone(&db), admin_receiver);

        // Set emergency access for the conduit user
        match set_emergency_access(&guard) {
            Ok(pwd_set) => {
                if pwd_set {
                    warn!("The Conduit account emergency password is set! Please unset it as soon as you finish admin account recovery!");
                    guard.admin.send_message(RoomMessageEventContent::text_plain("The Conduit account emergency password is set! Please unset it as soon as you finish admin account recovery!"));
                }
            }
            Err(e) => {
                error!(
                    "Could not set the configured emergency password for the conduit user: {}",
                    e
                )
            }
        };

        guard
            .sending
            .start_handler(Arc::clone(&db), sending_receiver);

        drop(guard);

        Self::start_cleanup_task(Arc::clone(&db), config).await;

        Ok(db)
    }

    #[cfg(feature = "conduit_bin")]
    pub async fn on_shutdown(db: Arc<TokioRwLock<Self>>) {
        info!(target: "shutdown-sync", "Received shutdown notification, notifying sync helpers...");
        db.read().await.globals.rotate.fire();
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
        futures.push(
            self.rooms
                .userroomid_notificationcount
                .watch_prefix(&userid_prefix),
        );
        futures.push(
            self.rooms
                .userroomid_highlightcount
                .watch_prefix(&userid_prefix),
        );

        // Events for rooms we are in
        for room_id in self.rooms.rooms_joined(user_id).filter_map(|r| r.ok()) {
            let short_roomid = self
                .rooms
                .get_shortroomid(&room_id)
                .ok()
                .flatten()
                .expect("room exists")
                .to_be_bytes()
                .to_vec();

            let roomid_bytes = room_id.as_bytes().to_vec();
            let mut roomid_prefix = roomid_bytes.clone();
            roomid_prefix.push(0xff);

            // PDUs
            futures.push(self.rooms.pduid_pdu.watch_prefix(&short_roomid));

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
                    .roomusertype_roomuserdataid
                    .watch_prefix(&roomuser_prefix),
            );
        }

        let mut globaluserdata_prefix = vec![0xff];
        globaluserdata_prefix.extend_from_slice(&userid_prefix);

        futures.push(
            self.account_data
                .roomusertype_roomuserdataid
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

    #[tracing::instrument(skip(self))]
    pub fn flush(&self) -> Result<()> {
        let start = std::time::Instant::now();

        let res = self._db.flush();

        debug!("flush: took {:?}", start.elapsed());

        res
    }

    #[tracing::instrument(skip(db, config))]
    pub async fn start_cleanup_task(db: Arc<TokioRwLock<Self>>, config: &Config) {
        use tokio::time::interval;

        #[cfg(unix)]
        use tokio::signal::unix::{signal, SignalKind};
        use tracing::info;

        use std::time::{Duration, Instant};

        let timer_interval = Duration::from_secs(config.cleanup_second_interval as u64);

        tokio::spawn(async move {
            let mut i = interval(timer_interval);
            #[cfg(unix)]
            let mut s = signal(SignalKind::hangup()).unwrap();

            loop {
                #[cfg(unix)]
                tokio::select! {
                    _ = i.tick() => {
                        info!("cleanup: Timer ticked");
                    }
                    _ = s.recv() => {
                        info!("cleanup: Received SIGHUP");
                    }
                };
                #[cfg(not(unix))]
                {
                    i.tick().await;
                    info!("cleanup: Timer ticked")
                }

                let start = Instant::now();
                if let Err(e) = db.read().await._db.cleanup() {
                    error!("cleanup: Errored: {}", e);
                } else {
                    info!("cleanup: Finished in {:?}", start.elapsed());
                }
            }
        });
    }
}

/// Sets the emergency password and push rules for the @conduit account in case emergency password is set
fn set_emergency_access(db: &KeyValueDatabase) -> Result<bool> {
    let conduit_user = UserId::parse_with_server_name("conduit", db.globals.server_name())
        .expect("@conduit:server_name is a valid UserId");

    db.users
        .set_password(&conduit_user, db.globals.emergency_password().as_deref())?;

    let (ruleset, res) = match db.globals.emergency_password() {
        Some(_) => (Ruleset::server_default(&conduit_user), Ok(true)),
        None => (Ruleset::new(), Ok(false)),
    };

    db.account_data.update(
        None,
        &conduit_user,
        GlobalAccountDataEventType::PushRules.to_string().into(),
        &GlobalAccountDataEvent {
            content: PushRulesEventContent { global: ruleset },
        },
        &db.globals,
    )?;

    res
}
