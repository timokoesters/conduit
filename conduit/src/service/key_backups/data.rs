use std::collections::BTreeMap;

use crate::Result;
use ruma::{
    OwnedRoomId, RoomId, UserId,
    api::client::backup::{BackupAlgorithm, KeyBackupData, RoomKeyBackup},
    serde::Raw,
};

pub trait Data: Send + Sync {
    fn create_backup(
        &self,
        user_id: &UserId,
        backup_metadata: &Raw<BackupAlgorithm>,
    ) -> Result<String>;

    fn delete_backup(&self, user_id: &UserId, version: &str) -> Result<()>;

    fn update_backup(
        &self,
        user_id: &UserId,
        version: &str,
        backup_metadata: &Raw<BackupAlgorithm>,
    ) -> Result<String>;

    fn get_latest_backup_version(&self, user_id: &UserId) -> Result<Option<String>>;

    fn get_latest_backup(&self, user_id: &UserId)
    -> Result<Option<(String, Raw<BackupAlgorithm>)>>;

    fn get_backup(&self, user_id: &UserId, version: &str) -> Result<Option<Raw<BackupAlgorithm>>>;

    fn add_key(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
        session_id: &str,
        key_data: &Raw<KeyBackupData>,
    ) -> Result<()>;

    fn count_keys(&self, user_id: &UserId, version: &str) -> Result<usize>;

    fn get_etag(&self, user_id: &UserId, version: &str) -> Result<String>;

    fn get_all(
        &self,
        user_id: &UserId,
        version: &str,
    ) -> Result<BTreeMap<OwnedRoomId, RoomKeyBackup>>;

    fn get_room(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
    ) -> Result<BTreeMap<String, Raw<KeyBackupData>>>;

    fn get_session(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<Option<Raw<KeyBackupData>>>;

    fn delete_all_keys(&self, user_id: &UserId, version: &str) -> Result<()>;

    fn delete_room_keys(&self, user_id: &UserId, version: &str, room_id: &RoomId) -> Result<()>;

    fn delete_room_key(
        &self,
        user_id: &UserId,
        version: &str,
        room_id: &RoomId,
        session_id: &str,
    ) -> Result<()>;
}
