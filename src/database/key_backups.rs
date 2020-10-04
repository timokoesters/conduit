use crate::{utils, Error, Result};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::backup::{BackupAlgorithm, KeyData, Sessions},
    },
    RoomId, UserId,
};
use std::{collections::BTreeMap, convert::TryFrom};

pub struct KeyBackups {
    pub(super) backupid_algorithm: sled::Tree, // BackupId = UserId + Version(Count)
    pub(super) backupid_etag: sled::Tree,      // BackupId = UserId + Version(Count)
    pub(super) backupkeyid_backup: sled::Tree, // BackupKeyId = UserId + Version + RoomId + SessionId
}

impl KeyBackups {
    pub fn create_backup(
        &self,
        user_id: &UserId,
        backup_metadata: &BackupAlgorithm,
        globals: &super::globals::Globals<'_>,
    ) -> Result<String> {
        let version = globals.next_count()?.to_string();

        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&version.as_bytes());

        self.backupid_algorithm.insert(
            &key,
            &*serde_json::to_string(backup_metadata)
                .expect("BackupAlgorithm::to_string always works"),
        )?;
        self.backupid_etag
            .insert(&key, &globals.next_count()?.to_be_bytes())?;
        Ok(version)
    }

    pub fn delete_backup(&self, user_id: &UserId, version: &str) -> Result<()> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&version.as_bytes());

        self.backupid_algorithm.remove(&key)?;
        self.backupid_etag.remove(&key)?;

        key.push(0xff);

        for outdated_key in self
            .backupkeyid_backup
            .scan_prefix(&key)
            .keys()
            .filter_map(|r| r.ok())
        {
            self.backupkeyid_backup.remove(outdated_key)?;
        }

        Ok(())
    }

    pub fn update_backup(
        &self,
        user_id: &UserId,
        version: &str,
        backup_metadata: &BackupAlgorithm,
        globals: &super::globals::Globals<'_>,
    ) -> Result<String> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&version.as_bytes());

        if self.backupid_algorithm.get(&key)?.is_none() {
            return Err(Error::BadRequest(
                ErrorKind::NotFound,
                "Tried to update nonexistent backup.",
            ));
        }

        self.backupid_algorithm.insert(
            &key,
            &*serde_json::to_string(backup_metadata)
                .expect("BackupAlgorithm::to_string always works"),
        )?;
        self.backupid_etag
            .insert(&key, &globals.next_count()?.to_be_bytes())?;
        Ok(version.to_string())
    }

    pub fn get_latest_backup(&self, user_id: &UserId) -> Result<Option<(String, BackupAlgorithm)>> {
        let mut prefix = user_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);
        self.backupid_algorithm
            .scan_prefix(&prefix)
            .last()
            .map_or(Ok(None), |r| {
                let (key, value) = r?;
                let version = utils::string_from_bytes(
                    key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| Error::bad_database("backupid_algorithm key is invalid."))?;

                Ok(Some((
                    version,
                    serde_json::from_slice(&value).map_err(|_| {
                        Error::bad_database("Algorithm in backupid_algorithm is invalid.")
                    })?,
                )))
            })
    }

    pub fn get_backup(&self, user_id: &UserId, version: &str) -> Result<Option<BackupAlgorithm>> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(version.as_bytes());

        self.backupid_algorithm.get(key)?.map_or(Ok(None), |bytes| {
            Ok(serde_json::from_slice(&bytes)
                .map_err(|_| Error::bad_database("Algorithm in backupid_algorithm is invalid."))?)
        })
    }

    pub fn add_key(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
        session_id: &str,
        key_data: &KeyData,
        globals: &super::globals::Globals<'_>,
    ) -> Result<()> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(version.as_bytes());

        if self.backupid_algorithm.get(&key)?.is_none() {
            return Err(Error::BadRequest(
                ErrorKind::NotFound,
                "Tried to update nonexistent backup.",
            ));
        }

        self.backupid_etag
            .insert(&key, &globals.next_count()?.to_be_bytes())?;

        key.push(0xff);
        key.extend_from_slice(room_id.to_string().as_bytes());
        key.push(0xff);
        key.extend_from_slice(session_id.as_bytes());

        self.backupkeyid_backup.insert(
            &key,
            &*serde_json::to_string(&key_data).expect("KeyData::to_string always works"),
        )?;

        Ok(())
    }

    pub fn count_keys(&self, user_id: &UserId, version: &str) -> Result<usize> {
        let mut prefix = user_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(version.as_bytes());

        Ok(self.backupkeyid_backup.scan_prefix(&prefix).count())
    }

    pub fn get_etag(&self, user_id: &UserId, version: &str) -> Result<String> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&version.as_bytes());

        Ok(utils::u64_from_bytes(
            &self
                .backupid_etag
                .get(&key)?
                .ok_or_else(|| Error::bad_database("Backup has no etag."))?,
        )
        .map_err(|_| Error::bad_database("etag in backupid_etag invalid."))?
        .to_string())
    }

    pub fn get_all(&self, user_id: &UserId, version: &str) -> Result<BTreeMap<RoomId, Sessions>> {
        let mut prefix = user_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(version.as_bytes());
        prefix.push(0xff);

        let mut rooms = BTreeMap::<RoomId, Sessions>::new();

        for result in self.backupkeyid_backup.scan_prefix(&prefix).map(|r| {
            let (key, value) = r?;
            let mut parts = key.rsplit(|&b| b == 0xff);

            let session_id = utils::string_from_bytes(
                &parts
                    .next()
                    .ok_or_else(|| Error::bad_database("backupkeyid_backup key is invalid."))?,
            )
            .map_err(|_| Error::bad_database("backupkeyid_backup session_id is invalid."))?;

            let room_id = RoomId::try_from(
                utils::string_from_bytes(
                    &parts
                        .next()
                        .ok_or_else(|| Error::bad_database("backupkeyid_backup key is invalid."))?,
                )
                .map_err(|_| Error::bad_database("backupkeyid_backup room_id is invalid."))?,
            )
            .map_err(|_| Error::bad_database("backupkeyid_backup room_id is invalid room id."))?;

            let key_data = serde_json::from_slice(&value)
                .map_err(|_| Error::bad_database("KeyData in backupkeyid_backup is invalid."))?;

            Ok::<_, Error>((room_id, session_id, key_data))
        }) {
            let (room_id, session_id, key_data) = result?;
            rooms
                .entry(room_id)
                .or_insert_with(|| Sessions {
                    sessions: BTreeMap::new(),
                })
                .sessions
                .insert(session_id, key_data);
        }

        Ok(rooms)
    }

    pub fn get_room(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
    ) -> BTreeMap<String, KeyData> {
        let mut prefix = user_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(version.as_bytes());
        prefix.push(0xff);
        prefix.extend_from_slice(room_id.as_bytes());
        prefix.push(0xff);

        self.backupkeyid_backup
            .scan_prefix(&prefix)
            .map(|r| {
                let (key, value) = r?;
                let mut parts = key.rsplit(|&b| b == 0xff);

                let session_id =
                    utils::string_from_bytes(&parts.next().ok_or_else(|| {
                        Error::bad_database("backupkeyid_backup key is invalid.")
                    })?)
                    .map_err(|_| {
                        Error::bad_database("backupkeyid_backup session_id is invalid.")
                    })?;

                let key_data = serde_json::from_slice(&value).map_err(|_| {
                    Error::bad_database("KeyData in backupkeyid_backup is invalid.")
                })?;

                Ok::<_, Error>((session_id, key_data))
            })
            .filter_map(|r| r.ok())
            .collect()
    }

    pub fn get_session(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<KeyData>> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(version.as_bytes());
        key.push(0xff);
        key.extend_from_slice(room_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(session_id.as_bytes());

        self.backupkeyid_backup
            .get(&key)?
            .map(|value| {
                serde_json::from_slice(&value)
                    .map_err(|_| Error::bad_database("KeyData in backupkeyid_backup is invalid."))
            })
            .transpose()
    }

    pub fn delete_all_keys(&self, user_id: &UserId, version: &str) -> Result<()> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&version.as_bytes());
        key.push(0xff);

        for outdated_key in self
            .backupkeyid_backup
            .scan_prefix(&key)
            .keys()
            .filter_map(|r| r.ok())
        {
            self.backupkeyid_backup.remove(outdated_key)?;
        }

        Ok(())
    }

    pub fn delete_room_keys(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
    ) -> Result<()> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&version.as_bytes());
        key.push(0xff);
        key.extend_from_slice(&room_id.as_bytes());
        key.push(0xff);

        for outdated_key in self
            .backupkeyid_backup
            .scan_prefix(&key)
            .keys()
            .filter_map(|r| r.ok())
        {
            self.backupkeyid_backup.remove(outdated_key)?;
        }

        Ok(())
    }

    pub fn delete_room_key(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<()> {
        let mut key = user_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&version.as_bytes());
        key.push(0xff);
        key.extend_from_slice(&room_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(&session_id.as_bytes());

        for outdated_key in self
            .backupkeyid_backup
            .scan_prefix(&key)
            .keys()
            .filter_map(|r| r.ok())
        {
            self.backupkeyid_backup.remove(outdated_key)?;
        }

        Ok(())
    }
}
