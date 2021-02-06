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
use log::info;
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
    #[serde(default)]
    allow_registration: bool,
    #[serde(default = "true_fn")]
    allow_encryption: bool,
    #[serde(default = "false_fn")]
    allow_federation: bool,
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
    4
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
            .open()?;

        info!("Opened sled database at {}", config.database_path);

        let (admin_sender, admin_receiver) = mpsc::unbounded();

        let db = Self {
            globals: globals::Globals::load(
                db.open_tree("global")?,
                db.open_tree("servertimeout_signingkey")?,
                config,
            )?,
            users: users::Users {
                userid_password: db.open_tree("userid_password")?,
                userid_displayname: db.open_tree("userid_displayname")?,
                userid_avatarurl: db.open_tree("userid_avatarurl")?,
                userdeviceid_token: db.open_tree("userdeviceid_token")?,
                userdeviceid_metadata: db.open_tree("userdeviceid_metadata")?,
                token_userdeviceid: db.open_tree("token_userdeviceid")?,
                onetimekeyid_onetimekeys: db.open_tree("onetimekeyid_onetimekeys")?,
                userid_lastonetimekeyupdate: db.open_tree("userid_lastonetimekeyupdate")?,
                keychangeid_userid: db.open_tree("devicekeychangeid_userid")?,
                keyid_key: db.open_tree("keyid_key")?,
                userid_masterkeyid: db.open_tree("userid_masterkeyid")?,
                userid_selfsigningkeyid: db.open_tree("userid_selfsigningkeyid")?,
                userid_usersigningkeyid: db.open_tree("userid_usersigningkeyid")?,
                todeviceid_events: db.open_tree("todeviceid_events")?,
            },
            uiaa: uiaa::Uiaa {
                userdeviceid_uiaainfo: db.open_tree("userdeviceid_uiaainfo")?,
            },
            rooms: rooms::Rooms {
                edus: rooms::RoomEdus {
                    readreceiptid_readreceipt: db.open_tree("readreceiptid_readreceipt")?,
                    roomuserid_privateread: db.open_tree("roomuserid_privateread")?, // "Private" read receipt
                    roomuserid_lastprivatereadupdate: db
                        .open_tree("roomid_lastprivatereadupdate")?,
                    typingid_userid: db.open_tree("typingid_userid")?,
                    roomid_lasttypingupdate: db.open_tree("roomid_lasttypingupdate")?,
                    presenceid_presence: db.open_tree("presenceid_presence")?,
                    userid_lastpresenceupdate: db.open_tree("userid_lastpresenceupdate")?,
                },
                pduid_pdu: db.open_tree("pduid_pdu")?,
                eventid_pduid: db.open_tree("eventid_pduid")?,
                roomid_pduleaves: db.open_tree("roomid_pduleaves")?,

                alias_roomid: db.open_tree("alias_roomid")?,
                aliasid_alias: db.open_tree("alias_roomid")?,
                publicroomids: db.open_tree("publicroomids")?,

                tokenids: db.open_tree("tokenids")?,

                roomserverids: db.open_tree("roomserverids")?,
                userroomid_joined: db.open_tree("userroomid_joined")?,
                roomuserid_joined: db.open_tree("roomuserid_joined")?,
                roomuseroncejoinedids: db.open_tree("roomuseroncejoinedids")?,
                userroomid_invited: db.open_tree("userroomid_invited")?,
                roomuserid_invited: db.open_tree("roomuserid_invited")?,
                userroomid_left: db.open_tree("userroomid_left")?,

                statekey_short: db.open_tree("statekey_short")?,
                stateid_pduid: db.open_tree("stateid_pduid")?,
                pduid_statehash: db.open_tree("pduid_statehash")?,
                roomid_statehash: db.open_tree("roomid_statehash")?,
                pduid_outlierpdu: db.open_tree("pduid_outlierpdu")?,
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
                backupkeyid_backup: db.open_tree("backupkeyid_backupmetadata")?,
            },
            transaction_ids: transaction_ids::TransactionIds {
                userdevicetxnid_response: db.open_tree("userdevicetxnid_response")?,
            },
            sending: sending::Sending {
                servernamepduids: db.open_tree("servernamepduids")?,
                servercurrentpdus: db.open_tree("servercurrentpdus")?,
                maximum_requests: Arc::new(Semaphore::new(10)),
            },
            admin: admin::Admin {
                sender: admin_sender,
            },
            appservice: appservice::Appservice {
                cached_registrations: Arc::new(RwLock::new(HashMap::new())),
                id_appserviceregistrations: db.open_tree("id_appserviceregistrations")?,
            },
            pusher: pusher::PushData::new(&db)?,
            _db: db,
        };

        db.admin.start_handler(db.clone(), admin_receiver);

        Ok(db)
    }

    pub async fn watch(&self, user_id: &UserId, device_id: &DeviceId) {
        let userid_bytes = user_id.to_string().as_bytes().to_vec();
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
        futures.push(self.rooms.userroomid_invited.watch_prefix(&userid_prefix));
        futures.push(self.rooms.userroomid_left.watch_prefix(&userid_prefix));

        // Events for rooms we are in
        for room_id in self.rooms.rooms_joined(user_id).filter_map(|r| r.ok()) {
            let roomid_bytes = room_id.to_string().as_bytes().to_vec();
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
        self._db.flush_async().await?;
        Ok(())
    }
}
