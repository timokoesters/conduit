use std::{collections::BTreeMap, ops::Range};

use ruma::{api::client::error::ErrorKind, OwnedServerName, ServerName, UserId};
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

    fn purge_and_get_hashes(
        &self,
        media: &[(OwnedServerName, String)],
        force_filehash: bool,
    ) -> Vec<Result<String>> {
        let mut files = Vec::new();

        let purge = |mut value: Vec<u8>| {
            value.truncate(32);
            let sha256_digest = value;

            let sha256_hex = hex::encode(&sha256_digest);

            self.purge_filehash(sha256_digest, false)?;

            Ok(sha256_hex)
        };

        for (server_name, media_id) in media {
            if force_filehash {
                let mut key = server_name.as_bytes().to_vec();
                key.push(0xff);
                key.extend_from_slice(media_id.as_bytes());

                match self.servernamemediaid_metadata.get(&key) {
                    Ok(Some(value)) => {
                        files.push(purge(value));
                    }
                    Ok(None) => (),
                    Err(e) => {
                        files.push(Err(e));
                    }
                }

                key.push(0xff);
                for (_, value) in self.thumbnailid_metadata.scan_prefix(key) {
                    files.push(purge(value));
                }
            } else {
                match self.purge_mediaid(server_name, media_id, false) {
                    Ok(f) => {
                        files.append(&mut f.into_iter().map(Ok).collect());
                    }
                    Err(e) => files.push(Err(e)),
                }
            }
        }

        files
    }

    fn purge_and_get_hashes_from_user(
        &self,
        user_id: &UserId,
        force_filehash: bool,
        after: Option<u64>,
    ) -> Vec<Result<String>> {
        let mut files = Vec::new();
        let mut prefix = user_id.server_name().as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(user_id.localpart().as_bytes());
        prefix.push(0xff);

        let purge_filehash = |sha256_digest: Vec<u8>| {
            let sha256_hex = hex::encode(&sha256_digest);

            self.purge_filehash(sha256_digest, false)?;

            Ok(sha256_hex)
        };

        for (k, _) in self.servername_userlocalpart_mediaid.scan_prefix(prefix) {
            let metadata = || {
                let mut parts = k.rsplit(|&b| b == 0xff);
                let media_id_bytes = parts.next().ok_or_else(|| {
                    Error::bad_database(
                        "Invalid format for key of servername_userlocalpart_mediaid",
                    )
                })?;

                let media_id = utils::string_from_bytes(media_id_bytes).map_err(|_| {
                    Error::bad_database(
                        "Invalid media_id string in servername_userlocalpart_mediaid",
                    )
                })?;

                let mut key = user_id.server_name().as_bytes().to_vec();
                key.push(0xff);
                key.extend_from_slice(media_id.as_bytes());

                Ok((
                    self.servernamemediaid_metadata.get(&key)?.ok_or_else(|| {
                    error!(
                        "Missing metadata for \"mxc://{}/{media_id}\", despite storing it's uploader",
                        user_id.server_name()
                    );
                        Error::BadDatabase("Missing metadata for media id and server_name")
                    })?,
                    media_id,
                ))
            };

            let (mut metadata, media_id) = match metadata() {
                Ok(v) => v,
                Err(e) => {
                    files.push(Err(e));
                    continue;
                }
            };

            metadata.truncate(32);
            let sha256_digest = metadata;

            if let Some(after) = after {
                let metadata = match self
                    .filehash_metadata
                    .get(&sha256_digest)
                    .map(|opt| opt.map(FilehashMetadata::from_vec))
                {
                    Ok(Some(metadata)) => metadata,
                    // If the media has already been deleted, we shouldn't treat that as an error
                    Ok(None) => continue,
                    Err(e) => {
                        files.push(Err(e));
                        continue;
                    }
                };

                let creation = match metadata.creation(&sha256_digest) {
                    Ok(c) => c,
                    Err(e) => {
                        files.push(Err(e));
                        continue;
                    }
                };

                if creation < after {
                    continue;
                }
            }

            if force_filehash {
                files.push(purge_filehash(sha256_digest));

                let mut prefix = user_id.server_name().as_bytes().to_vec();
                prefix.push(0xff);
                prefix.extend_from_slice(media_id.as_bytes());
                prefix.push(0xff);
                for (_, mut metadata) in self.thumbnailid_metadata.scan_prefix(prefix) {
                    metadata.truncate(32);
                    let sha256_digest = metadata;
                    files.push(purge_filehash(sha256_digest));
                }
            } else {
                match self.purge_mediaid(user_id.server_name(), &media_id, false) {
                    Ok(f) => {
                        files.append(&mut f.into_iter().map(Ok).collect());
                    }
                    Err(e) => files.push(Err(e)),
                }
            }
        }

        files
    }

    fn purge_and_get_hashes_from_server(
        &self,
        server_name: &ServerName,
        force_filehash: bool,
        after: Option<u64>,
    ) -> Vec<Result<String>> {
        let mut prefix = server_name.as_bytes().to_vec();
        prefix.push(0xff);

        let mut files = Vec::new();

        // Purges all references to the given media in the database,
        // returning a Vec of hex sha256 digests
        let purge_sha256 = |files: &mut Vec<Result<String>>, mut metadata: Vec<u8>| {
            metadata.truncate(32);
            let sha256_digest = metadata;

            if let Some(after) = after {
                let Some(metadata) = self
                    .filehash_metadata
                    .get(&sha256_digest)?
                    .map(FilehashMetadata::from_vec)
                else {
                    // If the media has already been deleted, we shouldn't treat that as an error
                    return Ok(());
                };

                if metadata.creation(&sha256_digest)? < after {
                    return Ok(());
                }
            }

            let sha256_hex = hex::encode(&sha256_digest);

            self.purge_filehash(sha256_digest, false)?;

            files.push(Ok(sha256_hex));
            Ok(())
        };

        let purge_mediaid = |files: &mut Vec<Result<String>>, key: Vec<u8>| {
            let mut parts = key.split(|&b| b == 0xff);

            let server_name = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid format of metadata key"))
                .map(utils::string_from_bytes)?
                .map_err(|_| Error::bad_database("Invalid ServerName String in metadata key"))
                .map(OwnedServerName::try_from)?
                .map_err(|_| Error::bad_database("Invalid ServerName String in metadata key"))?;

            let media_id = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid format of metadata key"))
                .map(utils::string_from_bytes)?
                .map_err(|_| Error::bad_database("Invalid Media ID String in metadata key"))?;

            files.append(
                &mut self
                    .purge_mediaid(&server_name, &media_id, false)?
                    .into_iter()
                    .map(Ok)
                    .collect(),
            );

            Ok(())
        };

        for (key, value) in self
            .servernamemediaid_metadata
            .scan_prefix(prefix.clone())
            .chain(self.thumbnailid_metadata.scan_prefix(prefix.clone()))
        {
            if let Err(e) = if force_filehash {
                purge_sha256(&mut files, value)
            } else {
                purge_mediaid(&mut files, key)
            } {
                files.push(Err(e));
            }
        }

        files
    }
}

impl KeyValueDatabase {
    fn purge_mediaid(
        &self,
        server_name: &ServerName,
        media_id: &str,
        only_filehash_metadata: bool,
    ) -> Result<Vec<String>> {
        let mut files = Vec::new();

        let count_required_to_purge = if only_filehash_metadata { 1 } else { 0 };

        let mut key = server_name.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(media_id.as_bytes());

        if let Some(sha256_digest) = self.servernamemediaid_metadata.get(&key)?.map(|mut value| {
            value.truncate(32);
            value
        }) {
            if !only_filehash_metadata {
                if let Some(localpart) = self.servernamemediaid_userlocalpart.get(&key)? {
                    self.servernamemediaid_userlocalpart.remove(&key)?;

                    let mut key = server_name.as_bytes().to_vec();
                    key.push(0xff);
                    key.extend_from_slice(&localpart);
                    key.push(0xff);
                    key.extend_from_slice(media_id.as_bytes());

                    self.servername_userlocalpart_mediaid.remove(&key)?;
                };

                self.servernamemediaid_metadata.remove(&key)?;

                let mut key = sha256_digest.clone();
                key.extend_from_slice(server_name.as_bytes());
                key.push(0xff);
                key.extend_from_slice(media_id.as_bytes());

                self.filehash_servername_mediaid.remove(&key)?;
            }

            if self
                .filehash_servername_mediaid
                .scan_prefix(sha256_digest.clone())
                .count()
                <= count_required_to_purge
                && self
                    .filehash_thumbnailid
                    .scan_prefix(sha256_digest.clone())
                    .next()
                    .is_none()
            {
                self.filehash_metadata.remove(&sha256_digest)?;
                files.push(hex::encode(sha256_digest));
            }
        }

        key.push(0xff);

        let mut thumbnails = BTreeMap::new();

        for (thumbnail_id, mut value) in self.thumbnailid_metadata.scan_prefix(key) {
            value.truncate(32);
            let sha256_digest = value;

            let entry = thumbnails
                .entry(sha256_digest.clone())
                .and_modify(|v| *v += 1)
                .or_insert(1);

            if !only_filehash_metadata {
                self.filehash_thumbnailid.remove(&sha256_digest)?;
                self.thumbnailid_metadata.remove(&thumbnail_id)?;
            }

            // Basically, if this is the only media pointing to the filehash, get rid of it.
            // It's a little complicated due to how blocking works.
            if self
                .filehash_servername_mediaid
                .scan_prefix(sha256_digest.clone())
                .count()
                <= count_required_to_purge
                && self
                    .filehash_thumbnailid
                    .scan_prefix(sha256_digest.clone())
                    .count()
                    <= if only_filehash_metadata { *entry } else { 0 }
            {
                self.filehash_metadata.remove(&sha256_digest)?;
                files.push(hex::encode(sha256_digest));
            }
        }

        Ok(files)
    }

    fn purge_filehash(&self, sha256_digest: Vec<u8>, only_filehash_metadata: bool) -> Result<()> {
        let handle_error = || {
            error!(
                "Invalid format of key in filehash_servername_mediaid for media with sha256 content hash of {}",
                hex::encode(&sha256_digest)
            );
            Error::BadDatabase("Invalid format of key in filehash_servername_mediaid")
        };

        if !only_filehash_metadata {
            for (key, _) in self.filehash_thumbnailid.scan_prefix(sha256_digest.clone()) {
                self.filehash_thumbnailid.remove(&key)?;
                let (_, key) = key.split_at(32);
                self.thumbnailid_metadata.remove(key)?;
            }

            for (k, _) in self
                .filehash_servername_mediaid
                .scan_prefix(sha256_digest.clone())
            {
                let (_, servername_mediaid) = k.split_at_checked(32).ok_or_else(handle_error)?;

                self.servernamemediaid_metadata.remove(servername_mediaid)?;
                self.filehash_servername_mediaid.remove(&k)?;

                if let Some(localpart) = self
                    .servernamemediaid_userlocalpart
                    .get(servername_mediaid)?
                {
                    self.servernamemediaid_userlocalpart
                        .remove(servername_mediaid)?;

                    let mut parts = servername_mediaid.split(|b: &u8| *b == 0xff);

                    let mut key = parts.next().ok_or_else(handle_error)?.to_vec();
                    key.push(0xff);
                    key.extend_from_slice(&localpart);
                    key.push(0xff);
                    key.extend_from_slice(parts.next().ok_or_else(handle_error)?);

                    self.servername_userlocalpart_mediaid.remove(&key)?;
                };
            }
        }

        self.filehash_metadata.remove(&sha256_digest)
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

    pub fn from_vec(vec: Vec<u8>) -> Self {
        Self { value: vec }
    }

    pub fn value(&self) -> &[u8] {
        &self.value
    }

    fn get_u64_val(
        &self,
        range: Range<usize>,
        name: &str,
        sha256_digest: &[u8],
        invalid_error: &'static str,
    ) -> Result<u64> {
        self.value
            .get(range)
            .ok_or_else(|| {
                error!(
                    "Invalid format of metadata for media with sha256 content hash of {}",
                    hex::encode(sha256_digest)
                );
                Error::BadDatabase("Invalid format of metadata in filehash_metadata")
            })?
            .try_into()
            .map(u64::from_be_bytes)
            .map_err(|_| {
                error!(
                    "Invalid {name} for media with sha256 content hash of {}",
                    hex::encode(sha256_digest)
                );
                Error::BadDatabase(invalid_error)
            })
    }

    pub fn creation(&self, sha256_digest: &[u8]) -> Result<u64> {
        self.get_u64_val(
            8..16,
            "creation time",
            sha256_digest,
            "Invalid creation time in filehash_metadata",
        )
    }
}
