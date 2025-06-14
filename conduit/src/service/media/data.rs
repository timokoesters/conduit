use ruma::{OwnedServerName, ServerName, UserId};
use sha2::{Sha256, digest::Output};

use crate::{Error, Result, config::MediaRetentionConfig, service::media::FileInfo};

use super::{
    BlockedMediaInfo, DbFileMeta, MediaListItem, MediaQuery, MediaType, ServerNameOrUserId,
};

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

    fn query(&self, server_name: &ServerName, media_id: &str) -> Result<MediaQuery>;

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

    fn list(
        &self,
        server_name_or_user_id: Option<ServerNameOrUserId>,
        include_thumbnails: bool,
        content_type: Option<&str>,
        before: Option<u64>,
        after: Option<u64>,
    ) -> Result<Vec<MediaListItem>>;

    /// Returns a Vec of:
    /// - The server the media is from
    /// - The media id
    /// - The time it was blocked, in unix seconds
    /// - The optional reason why it was blocked
    fn list_blocked(&self) -> Vec<Result<BlockedMediaInfo>>;

    fn is_blocked_filehash(&self, sha256_digest: &[u8]) -> Result<bool>;

    /// Gets the files that need to be deleted from the media backend in order to meet the `space`
    /// requirements, as specified in the retention config. Calling this also causes those files'
    /// metadata to be deleted from the database.
    fn files_to_delete(
        &self,
        sha256_digest: &[u8],
        retention: &MediaRetentionConfig,
        media_type: MediaType,
        new_size: u64,
    ) -> Result<Vec<Result<String>>>;

    /// Gets the files that need to be deleted from the media backend in order to meet the
    /// time-based requirements (`created` and `accessed`), as specified in the retention config.
    /// Calling this also causes those files' metadata to be deleted from the database.
    fn cleanup_time_retention(&self, retention: &MediaRetentionConfig) -> Vec<Result<String>>;

    fn update_last_accessed(&self, server_name: &ServerName, media_id: &str) -> Result<()>;

    fn update_last_accessed_filehash(&self, sha256_digest: &[u8]) -> Result<()>;

    /// Returns the known information about a file
    fn file_info(&self, sha256_digest: &[u8]) -> Result<Option<FileInfo>>;
}
