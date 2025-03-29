use ruma::{api::client::error::ErrorKind, ServerName, UserId};
use sha2::{digest::Output, Sha256};
use tracing::error;

use crate::{
    database::KeyValueDatabase,
    service::{self, media::DbFileMeta},
    utils, Error, Result,
};

impl service::media::Data for KeyValueDatabase {
    fn create_file_metadata(
        &self,
        sha256_digest: Output<Sha256>,
        file_size: u64,
        servername: &ServerName,
        media_id: &str,
        filename: Option<&str>,
        content_type: Option<&str>,
        user_id: Option<&UserId>,
    ) -> Result<()> {
        let metadata = FilehashMetadata::new(file_size);

        self.filehash_metadata
            .insert(&sha256_digest, metadata.value())?;

        let mut key = sha256_digest.to_vec();
        key.extend_from_slice(servername.as_bytes());
        key.push(0xff);
        key.extend_from_slice(media_id.as_bytes());

        self.filehash_servername_mediaid.insert(&key, &[])?;

        let mut key = servername.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(media_id.as_bytes());

        let mut value = sha256_digest.to_vec();
        value.extend_from_slice(filename.map(|f| f.as_bytes()).unwrap_or_default());
        value.push(0xff);
        value.extend_from_slice(content_type.map(|f| f.as_bytes()).unwrap_or_default());

        self.servernamemediaid_metadata.insert(&key, &value)?;

        if let Some(user_id) = user_id {
            let mut key = servername.as_bytes().to_vec();
            key.push(0xff);
            key.extend_from_slice(user_id.localpart().as_bytes());
            key.push(0xff);
            key.extend_from_slice(media_id.as_bytes());

            self.servername_userlocalpart_mediaid.insert(&key, &[])?;

            let mut key = servername.as_bytes().to_vec();
            key.push(0xff);
            key.extend_from_slice(media_id.as_bytes());

            self.servernamemediaid_userlocalpart
                .insert(&key, user_id.localpart().as_bytes())?;
        }

        Ok(())
    }

    fn search_file_metadata(&self, servername: &ServerName, media_id: &str) -> Result<DbFileMeta> {
        let mut key = servername.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(media_id.as_bytes());

        let value = self
            .servernamemediaid_metadata
            .get(&key)?
            .ok_or_else(|| Error::BadRequest(ErrorKind::NotFound, "Media not found."))?;

        let metadata = parse_metadata(&value).inspect_err(|e| {
            error!("Error parsing metadata for \"mxc://{servername}/{media_id}\" from servernamemediaid_metadata: {e}");
        })?;

        // Only assume file is available if there is metadata about the filehash itself
        self.filehash_metadata
            .get(&metadata.sha256_digest)?
            .map(|_| metadata)
            .ok_or_else(|| Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }

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
    ) -> Result<()> {
        let metadata = FilehashMetadata::new(file_size);

        self.filehash_metadata
            .insert(&sha256_digest, metadata.value())?;

        let mut key = sha256_digest.to_vec();
        key.extend_from_slice(servername.as_bytes());
        key.push(0xff);
        key.extend_from_slice(media_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(&width.to_be_bytes());
        key.extend_from_slice(&height.to_be_bytes());

        self.filehash_thumbnailid.insert(&key, &[])?;

        let mut key = servername.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(media_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(&width.to_be_bytes());
        key.extend_from_slice(&height.to_be_bytes());

        let mut value = sha256_digest.to_vec();
        value.extend_from_slice(filename.map(|f| f.as_bytes()).unwrap_or_default());
        value.push(0xff);
        value.extend_from_slice(content_type.map(|f| f.as_bytes()).unwrap_or_default());

        self.thumbnailid_metadata.insert(&key, &value)
    }

    fn search_thumbnail_metadata(
        &self,
        servername: &ServerName,
        media_id: &str,
        width: u32,
        height: u32,
    ) -> Result<DbFileMeta> {
        let mut key = servername.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(media_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(&width.to_be_bytes());
        key.extend_from_slice(&height.to_be_bytes());

        let value = self
            .thumbnailid_metadata
            .get(&key)?
            .ok_or_else(|| Error::BadRequest(ErrorKind::NotFound, "Media not found."))?;

        let metadata = parse_metadata(&value).inspect_err(|e| {
            error!("Error parsing metadata for thumbnail \"mxc://{servername}/{media_id}\" with dimensions {width}x{height} from thumbnailid_metadata: {e}");
        })?;

        // Only assume file is available if there is metadata about the filehash itself
        self.filehash_metadata
            .get(&metadata.sha256_digest)?
            .map(|_| metadata)
            .ok_or_else(|| Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}

fn parse_metadata(value: &[u8]) -> Result<DbFileMeta> {
    let (sha256_digest, mut parts) = value
        .split_at_checked(32)
        .map(|(digest, value)| (digest.to_vec(), value.split(|&b| b == 0xff)))
        .ok_or_else(|| Error::BadDatabase("Invalid format for media metadata"))?;

    let filename = parts
        .next()
        .map(|bytes| {
            utils::string_from_bytes(bytes)
                .map_err(|_| Error::BadDatabase("filename in media metadata is invalid unicode"))
        })
        .transpose()?
        .and_then(|s| (!s.is_empty()).then_some(s));

    let content_type = parts
        .next()
        .map(|bytes| {
            utils::string_from_bytes(bytes).map_err(|_| {
                Error::BadDatabase("content type in media metadata is invalid unicode")
            })
        })
        .transpose()?
        .and_then(|s| (!s.is_empty()).then_some(s));

    let unauthenticated_access_permitted = parts.next().is_some_and(|v| v.is_empty());

    Ok(DbFileMeta {
        sha256_digest,
        filename,
        content_type,
        unauthenticated_access_permitted,
    })
}

pub struct FilehashMetadata {
    value: Vec<u8>,
}

impl FilehashMetadata {
    pub fn new_with_times(size: u64, creation: u64, last_access: u64) -> Self {
        let mut value = size.to_be_bytes().to_vec();
        value.extend_from_slice(&creation.to_be_bytes());
        value.extend_from_slice(&last_access.to_be_bytes());

        Self { value }
    }

    pub fn new(size: u64) -> Self {
        let now = utils::secs_since_unix_epoch();

        let mut value = size.to_be_bytes().to_vec();
        value.extend_from_slice(&now.to_be_bytes());
        value.extend_from_slice(&now.to_be_bytes());

        Self { value }
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }
}
