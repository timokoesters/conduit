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

use crate::{Error, Result};
use directories::ProjectDirs;
use futures::StreamExt;
use log::{error, info};
use rocket::futures::{self, channel::mpsc};
use ruma::{DeviceId, ServerName, UserId};
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs::remove_dir_all,
    sync::{Arc, RwLock},
};
use tokio::sync::Semaphore;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    server_name: Box<ServerName>,
    database_path: String,
    #[serde(default = "default_cache_capacity")]
    cache_capacity: u32,
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
    jwt_secret: Option<String>,
    #[serde(default = "Vec::new")]
    trusted_servers: Vec<Box<ServerName>>,
    #[serde(default = "default_log")]
    pub log: String,
}

fn false_fn() -> bool {
    false
}

fn true_fn() -> bool {
    true
}

fn default_cache_capacity() -> u32 {
    1024 * 1024 * 1024
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

#[derive(Clone)]
pub struct Database {
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
    pub _db: sled::Db,
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
    pub async fn load_or_create(config: Config) -> Result<Self> {
        let db = sled::Config::default()
            .path(&config.database_path)
            .cache_capacity(config.cache_capacity as u64)
            .use_compression(true)
            .open()?;

        if config.max_request_size < 1024 {
            eprintln!("ERROR: Max request size is less than 1KB. Please increase it.");
        }

        let (admin_sender, admin_receiver) = mpsc::unbounded();

        let db = Self {
            users: users::Users {
                userid_password: db.open_tree("userid_password")?,
                userid_displayname: db.open_tree("userid_displayname")?,
                userid_avatarurl: db.open_tree("userid_avatarurl")?,
                userdeviceid_token: db.open_tree("userdeviceid_token")?,
                userdeviceid_metadata: db.open_tree("userdeviceid_metadata")?,
                userid_devicelistversion: db.open_tree("userid_devicelistversion")?,
                token_userdeviceid: db.open_tree("token_userdeviceid")?,
                onetimekeyid_onetimekeys: db.open_tree("onetimekeyid_onetimekeys")?,
                userid_lastonetimekeyupdate: db.open_tree("userid_lastonetimekeyupdate")?,
                keychangeid_userid: db.open_tree("keychangeid_userid")?,
                keyid_key: db.open_tree("keyid_key")?,
                userid_masterkeyid: db.open_tree("userid_masterkeyid")?,
                userid_selfsigningkeyid: db.open_tree("userid_selfsigningkeyid")?,
                userid_usersigningkeyid: db.open_tree("userid_usersigningkeyid")?,
                todeviceid_events: db.open_tree("todeviceid_events")?,
            },
            uiaa: uiaa::Uiaa {
                userdevicesessionid_uiaainfo: db.open_tree("userdevicesessionid_uiaainfo")?,
                userdevicesessionid_uiaarequest: db.open_tree("userdevicesessionid_uiaarequest")?,
            },
            rooms: rooms::Rooms {
                edus: rooms::RoomEdus {
                    readreceiptid_readreceipt: db.open_tree("readreceiptid_readreceipt")?,
                    roomuserid_privateread: db.open_tree("roomuserid_privateread")?, // "Private" read receipt
                    roomuserid_lastprivatereadupdate: db
                        .open_tree("roomuserid_lastprivatereadupdate")?,
                    typingid_userid: db.open_tree("typingid_userid")?,
                    roomid_lasttypingupdate: db.open_tree("roomid_lasttypingupdate")?,
                    presenceid_presence: db.open_tree("presenceid_presence")?,
                    userid_lastpresenceupdate: db.open_tree("userid_lastpresenceupdate")?,
                },
                pduid_pdu: db.open_tree("pduid_pdu")?,
                eventid_pduid: db.open_tree("eventid_pduid")?,
                roomid_pduleaves: db.open_tree("roomid_pduleaves")?,

                alias_roomid: db.open_tree("alias_roomid")?,
                aliasid_alias: db.open_tree("aliasid_alias")?,
                publicroomids: db.open_tree("publicroomids")?,

                tokenids: db.open_tree("tokenids")?,

                roomserverids: db.open_tree("roomserverids")?,
                serverroomids: db.open_tree("serverroomids")?,
                userroomid_joined: db.open_tree("userroomid_joined")?,
                roomuserid_joined: db.open_tree("roomuserid_joined")?,
                roomuseroncejoinedids: db.open_tree("roomuseroncejoinedids")?,
                userroomid_invitestate: db.open_tree("userroomid_invitestate")?,
                roomuserid_invitecount: db.open_tree("roomuserid_invitecount")?,
                userroomid_leftstate: db.open_tree("userroomid_leftstate")?,
                roomuserid_leftcount: db.open_tree("roomuserid_leftcount")?,

                userroomid_notificationcount: db.open_tree("userroomid_notificationcount")?,
                userroomid_highlightcount: db.open_tree("userroomid_highlightcount")?,

                statekey_shortstatekey: db.open_tree("statekey_shortstatekey")?,
                stateid_shorteventid: db.open_tree("stateid_shorteventid")?,
                eventid_shorteventid: db.open_tree("eventid_shorteventid")?,
                shorteventid_eventid: db.open_tree("shorteventid_eventid")?,
                shorteventid_shortstatehash: db.open_tree("shorteventid_shortstatehash")?,
                roomid_shortstatehash: db.open_tree("roomid_shortstatehash")?,
                statehash_shortstatehash: db.open_tree("statehash_shortstatehash")?,

                eventid_outlierpdu: db.open_tree("eventid_outlierpdu")?,
                prevevent_parent: db.open_tree("prevevent_parent")?,
            },
            account_data: account_data::AccountData {
                roomuserdataid_accountdata: db.open_tree("roomuserdataid_accountdata")?,
            },
            media: media::Media {
                mediaid_file: db.open_tree("mediaid_file")?,
            },
            key_backups: key_backups::KeyBackups {
                backupid_algorithm: db.open_tree("backupid_algorithm")?,
                backupid_etag: db.open_tree("backupid_etag")?,
                backupkeyid_backup: db.open_tree("backupkeyid_backup")?,
            },
            transaction_ids: transaction_ids::TransactionIds {
                userdevicetxnid_response: db.open_tree("userdevicetxnid_response")?,
            },
            sending: sending::Sending {
                servername_educount: db.open_tree("servername_educount")?,
                servernamepduids: db.open_tree("servernamepduids")?,
                servercurrentevents: db.open_tree("servercurrentevents")?,
                maximum_requests: Arc::new(Semaphore::new(config.max_concurrent_requests as usize)),
            },
            admin: admin::Admin {
                sender: admin_sender,
            },
            appservice: appservice::Appservice {
                cached_registrations: Arc::new(RwLock::new(HashMap::new())),
                id_appserviceregistrations: db.open_tree("id_appserviceregistrations")?,
            },
            pusher: pusher::PushData::new(&db)?,
            globals: globals::Globals::load(
                db.open_tree("global")?,
                db.open_tree("server_signingkeys")?,
                config,
            )?,
            _db: db,
        };

        // MIGRATIONS
        if db.globals.database_version()? < 1 {
            for roomserverid in db.rooms.roomserverids.iter().keys() {
                let roomserverid = roomserverid?;
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

                db.rooms.serverroomids.insert(serverroomid, &[])?;
            }

            db.globals.bump_database_version(1)?;

            info!("Migration: 0 -> 1 finished");
        }

        // This data is probably outdated
        db.rooms.edus.presenceid_presence.clear()?;

        db.admin.start_handler(db.clone(), admin_receiver);

        Ok(db)
    }

    pub async fn watch(&self, user_id: &UserId, device_id: &DeviceId) {
        let userid_bytes = user_id.as_bytes().to_vec();
        let mut userid_prefix = userid_bytes.clone();
        userid_prefix.push(0xff);

        let mut userdeviceid_prefix = userid_prefix.clone();
        userdeviceid_prefix.extend_from_slice(device_id.as_bytes());
        userdeviceid_prefix.push(0xff);

        let mut futures = futures::stream::FuturesUnordered::new();

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

        // Wait until one of them finds something
        futures.next().await;
    }

    pub async fn flush(&self) -> Result<()> {
        // noop while we don't use sled 1.0
        //self._db.flush_async().await?;
        Ok(())
    }
}
