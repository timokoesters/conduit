use ruma::{OwnedServerName, OwnedUserId, ServerName, http_headers::ContentDisposition};

pub struct DbFileMeta {
    pub sha256_digest: Vec<u8>,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub unauthenticated_access_permitted: bool,
}

pub struct Query {
    pub is_blocked: bool,
    pub source_file: Option<QueryFileInfo>,
    pub thumbnails: Vec<QueryThumbInfo>,
}

pub struct QueryFileInfo {
    pub uploader_localpart: Option<String>,
    pub sha256_hex: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub unauthenticated_access_permitted: bool,
    pub is_blocked_via_filehash: bool,
    pub file_info: Option<FileInfo>,
}

pub struct QueryThumbInfo {
    pub width: u32,
    pub height: u32,
    pub sha256_hex: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub unauthenticated_access_permitted: bool,
    pub is_blocked_via_filehash: bool,
    pub file_info: Option<FileInfo>,
}

pub struct FileInfo {
    pub creation: u64,
    pub last_access: u64,
    pub size: u64,
}

pub struct ListItem {
    pub server_name: OwnedServerName,
    pub media_id: String,
    pub uploader_localpart: Option<String>,
    pub content_type: Option<String>,
    pub filename: Option<String>,
    pub dimensions: Option<(u32, u32)>,
    pub size: u64,
    pub creation: u64,
}

pub enum ServerNameOrUserId {
    ServerName(OwnedServerName),
    UserId(OwnedUserId),
}

pub struct FileMeta {
    pub content_disposition: ContentDisposition,
    pub content_type: Option<String>,
    pub file: Vec<u8>,
}

pub struct Type {
    pub local: bool,
    pub thumbnail: bool,
}

impl Type {
    pub fn new(
        self_server_name: &ServerName,
        media_server_name: &ServerName,
        thumbnail: bool,
    ) -> Self {
        Self {
            local: self_server_name == media_server_name,
            thumbnail,
        }
    }
}

pub struct BlockedMediaInfo {
    pub server_name: OwnedServerName,
    pub media_id: String,
    pub unix_secs: u64,
    pub reason: Option<String>,
    pub sha256_hex: Option<String>,
}
