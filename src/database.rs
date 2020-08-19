pub mod account_data;
pub mod globals;
pub mod key_backups;
pub mod media;
pub mod rooms;
pub mod uiaa;
pub mod users;

use crate::{Error, Result};
use directories::ProjectDirs;
use log::info;
use std::fs::remove_dir_all;

use futures::StreamExt;
use rocket::{futures, Config};
use ruma::{DeviceId, UserId};

pub struct Database {
    pub globals: globals::Globals,
    pub users: users::Users,
    pub uiaa: uiaa::Uiaa,
    pub rooms: rooms::Rooms,
    pub account_data: account_data::AccountData,
    pub media: media::Media,
    pub key_backups: key_backups::KeyBackups,
    pub _db: sled::Db,
}

impl Database {
    /// Tries to remove the old database but ignores all errors.
    pub fn try_remove(server_name: &str) -> Result<()> {
        let mut path = ProjectDirs::from("xyz", "koesters", "conduit")
            .ok_or(Error::BadConfig(
                "The OS didn't return a valid home directory path.",
            ))?
            .data_dir()
            .to_path_buf();
        path.push(server_name);
        let _ = remove_dir_all(path);

        Ok(())
    }

    /// Load an existing database or create a new one.
    pub fn load_or_create(config: &Config) -> Result<Self> {
        let server_name = config.get_str("server_name").unwrap_or("localhost");

        let path = config
            .get_str("database_path")
            .map(|x| Ok::<_, Error>(x.to_owned()))
            .unwrap_or_else(|_| {
                let path = ProjectDirs::from("xyz", "koesters", "conduit")
                    .ok_or(Error::BadConfig(
                        "The OS didn't return a valid home directory path.",
                    ))?
                    .data_dir()
                    .join(server_name);

                Ok(path
                    .to_str()
                    .ok_or(Error::BadConfig("Database path contains invalid unicode."))?
                    .to_owned())
            })?;

        let db = sled::open(&path)?;
        info!("Opened sled database at {}", path);

        Ok(Self {
            globals: globals::Globals::load(db.open_tree("global")?, config)?,
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
                    roomuserid_lastread: db.open_tree("roomuserid_lastread")?, // "Private" read receipt
                    roomlatestid_roomlatest: db.open_tree("roomlatestid_roomlatest")?, // Read receipts
                    roomactiveid_userid: db.open_tree("roomactiveid_userid")?, // Typing notifs
                    roomid_lastroomactiveupdate: db.open_tree("roomid_lastroomactiveupdate")?,
                    presenceid_presence: db.open_tree("presenceid_presence")?,
                    userid_lastpresenceupdate: db.open_tree("userid_lastpresenceupdate")?,
                },
                pduid_pdu: db.open_tree("pduid_pdu")?,
                eventid_pduid: db.open_tree("eventid_pduid")?,
                roomstateid_pduid: db.open_tree("roomstateid_pduid")?,
                roomid_pduleaves: db.open_tree("roomid_pduleaves")?,

                alias_roomid: db.open_tree("alias_roomid")?,
                aliasid_alias: db.open_tree("alias_roomid")?,
                publicroomids: db.open_tree("publicroomids")?,

                userroomid_joined: db.open_tree("userroomid_joined")?,
                roomuserid_joined: db.open_tree("roomuserid_joined")?,
                userroomid_invited: db.open_tree("userroomid_invited")?,
                roomuserid_invited: db.open_tree("roomuserid_invited")?,
                userroomid_left: db.open_tree("userroomid_left")?,

                stateid_pduid: db.open_tree("stateid_pduid")?,
                pduid_statehash: db.open_tree("pduid_statehash")?,
                roomid_statehash: db.open_tree("roomid_statehash")?,
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
            _db: db,
        })
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
            let mut roomid_prefix = room_id.to_string().as_bytes().to_vec();
            roomid_prefix.push(0xff);

            // PDUs
            futures.push(self.rooms.pduid_pdu.watch_prefix(&roomid_prefix));

            // EDUs
            futures.push(
                self.rooms
                    .edus
                    .roomid_lastroomactiveupdate
                    .watch_prefix(&roomid_prefix),
            );

            futures.push(
                self.rooms
                    .edus
                    .roomlatestid_roomlatest
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
}
