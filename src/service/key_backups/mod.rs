mod data;
pub use data::Data;

use crate::Result;
use ruma::{
    api::client::backup::{BackupAlgorithm, KeyBackupData, RoomKeyBackup},
    serde::Raw,
    RoomId, UserId,
};
use std::collections::BTreeMap;

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    pub fn create_backup(
        &self,
        user_id: &UserId,
        backup_metadata: &Raw<BackupAlgorithm>,
    ) -> Result<String> {
        self.db.create_backup(user_id, backup_metadata)
    }

    pub fn delete_backup(&self, user_id: &UserId, version: &str) -> Result<()> {
        self.db.delete_backup(user_id, version)
    }

    pub fn update_backup(
        &self,
        user_id: &UserId,
        version: &str,
        backup_metadata: &Raw<BackupAlgorithm>,
    ) -> Result<String> {
        self.db.update_backup(user_id, version, backup_metadata)
    }

    pub fn get_latest_backup_version(&self, user_id: &UserId) -> Result<Option<String>> {
        self.db.get_latest_backup_version(user_id)
    }

    pub fn get_latest_backup(
        &self,
        user_id: &UserId,
    ) -> Result<Option<(String, Raw<BackupAlgorithm>)>> {
        self.db.get_latest_backup(user_id)
    }

    pub fn get_backup(
        &self,
        user_id: &UserId,
        version: &str,
    ) -> Result<Option<Raw<BackupAlgorithm>>> {
        self.db.get_backup(user_id, version)
    }

    pub fn add_key(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
        session_id: &str,
        key_data: &Raw<KeyBackupData>,
    ) -> Result<()> {
        self.db
            .add_key(user_id, version, room_id, session_id, key_data)
    }

    pub fn count_keys(&self, user_id: &UserId, version: &str) -> Result<usize> {
        self.db.count_keys(user_id, version)
    }

    pub fn get_etag(&self, user_id: &UserId, version: &str) -> Result<String> {
        self.db.get_etag(user_id, version)
    }

    pub fn get_all(
        &self,
        user_id: &UserId,
        version: &str,
    ) -> Result<BTreeMap<Box<RoomId>, RoomKeyBackup>> {
        self.db.get_all(user_id, version)
    }

    pub fn get_room(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
    ) -> Result<BTreeMap<String, Raw<KeyBackupData>>> {
        self.db.get_room(user_id, version, room_id)
    }

    pub fn get_session(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<Raw<KeyBackupData>>> {
        self.db.get_session(user_id, version, room_id, session_id)
    }

    pub fn delete_all_keys(&self, user_id: &UserId, version: &str) -> Result<()> {
        self.db.delete_all_keys(user_id, version)
    }

    pub fn delete_room_keys(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
    ) -> Result<()> {
        self.db.delete_room_keys(user_id, version, room_id)
    }

    pub fn delete_room_key(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<()> {
        self.db
            .delete_room_key(user_id, version, room_id, session_id)
    }
}
