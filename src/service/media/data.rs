use ruma::{ServerName, UserId};
use sha2::{digest::Output, Sha256};

use crate::Result;

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
}
