use ruma::{OwnedServerName, ServerName, UserId};
use sha2::{digest::Output, Sha256};

use crate::{Error, Result};

use super::BlockedMediaInfo;

use super::DbFileMeta;

pub trait Data: Send + Sync {
    #[allow(clippy::too_many_arguments)]
    fn create_file_metadata(
        &self,
        sha256_digest: Output<Sha256>,
        file_size: u64,
        servername: &ServerName,
        media_id: &str,
        filename: Option<&str>,
        content_type: Option<&str>,
        user_id: Option<&UserId>,
        is_blocked_filehash: bool,
    ) -> Result<()>;

    fn search_file_metadata(&self, servername: &ServerName, media_id: &str) -> Result<DbFileMeta>;

    #[allow(clippy::too_many_arguments)]
    fn create_thumbnail_metadata(
        &self,
        sha256_digest: Output<Sha256>,
        file_size: u64,
        servername: &ServerName,
        media_id: &str,
        width: u32,
        height: u32,
        filename: Option<&str>,
        content_type: Option<&str>,
    ) -> Result<()>;

    // Returns the sha256 hash, filename and content_type and whether the media should be accessible via
    /// unauthenticated endpoints.
    fn search_thumbnail_metadata(
        &self,
        servername: &ServerName,
        media_id: &str,
        width: u32,
        height: u32,
    ) -> Result<DbFileMeta>;

    fn purge_and_get_hashes(
        &self,
        media: &[(OwnedServerName, String)],
        force_filehash: bool,
    ) -> Vec<Result<String>>;

    fn purge_and_get_hashes_from_user(
        &self,
        user_id: &UserId,
        force_filehash: bool,
        after: Option<u64>,
    ) -> Vec<Result<String>>;

    fn purge_and_get_hashes_from_server(
        &self,
        server_name: &ServerName,
        force_filehash: bool,
        after: Option<u64>,
    ) -> Vec<Result<String>>;

    fn is_blocked(&self, server_name: &ServerName, media_id: &str) -> Result<bool>;

    fn block(
        &self,
        media: &[(OwnedServerName, String)],
        unix_secs: u64,
        reason: Option<String>,
    ) -> Vec<Error>;

    fn block_from_user(
        &self,
        user_id: &UserId,
        now: u64,
        reason: &str,
        after: Option<u64>,
    ) -> Vec<Error>;

    fn unblock(&self, media: &[(OwnedServerName, String)]) -> Vec<Error>;

    /// Returns a Vec of:
    /// - The server the media is from
    /// - The media id
    /// - The time it was blocked, in unix seconds
    /// - The optional reason why it was blocked
    fn list_blocked(&self) -> Vec<Result<BlockedMediaInfo>>;

    fn is_blocked_filehash(&self, sha256_digest: &[u8]) -> Result<bool>;
}
