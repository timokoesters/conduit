use std::{collections::BTreeMap, ops::Range, slice::Split};

use bytesize::ByteSize;
use ruma::{api::client::error::ErrorKind, OwnedServerName, ServerName, UserId};
use sha2::{digest::Output, Sha256};
use tracing::error;

use crate::{
    config::{MediaRetentionConfig, MediaRetentionScope},
    database::KeyValueDatabase,
    service::{
        self,
        media::{BlockedMediaInfo, Data as _, DbFileMeta, MediaType},
    },
    services, utils, Error, Result,
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
        is_blocked_filehash: bool,
    ) -> Result<()> {
        if !is_blocked_filehash {
            let metadata = FilehashMetadata::new(file_size);

            self.filehash_metadata
                .insert(&sha256_digest, metadata.value())?;
        };

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

            let is_blocked = self.is_blocked_filehash(&sha256_digest)?;
            let sha256_hex = hex::encode(&sha256_digest);

            // If the file is blocked, we want to keep the metadata about it so it can be viewed,
            // as well as filehashes blocked
            self.purge_filehash(sha256_digest, is_blocked)?;

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
                match self
                    .is_blocked(server_name, media_id)
                    .map(|is_blocked| self.purge_mediaid(server_name, media_id, is_blocked))
                {
                    Ok(Ok(f)) => {
                        files.append(&mut f.into_iter().map(Ok).collect());
                    }
                    Ok(Err(e)) | Err(e) => files.push(Err(e)),
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
            let is_blocked = self.is_blocked_filehash(&sha256_digest)?;

            // If the file is blocked, we want to keep the metadata about it so it can be viewed,
            // as well as filehashes blocked
            self.purge_filehash(sha256_digest, is_blocked)?;

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
                match self
                    .is_blocked(user_id.server_name(), &media_id)
                    .map(|is_blocked| {
                        self.purge_mediaid(user_id.server_name(), &media_id, is_blocked)
                    }) {
                    Ok(Ok(f)) => {
                        files.append(&mut f.into_iter().map(Ok).collect());
                    }
                    Ok(Err(e)) | Err(e) => files.push(Err(e)),
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
            let is_blocked = self.is_blocked_filehash(&sha256_digest)?;

            // If the file is blocked, we want to keep the metadata about it so it can be viewed,
            // as well as filehashes blocked
            self.purge_filehash(sha256_digest, is_blocked)?;

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

            let is_blocked = self.is_blocked(&server_name, &media_id)?;

            files.append(
                &mut self
                    .purge_mediaid(&server_name, &media_id, is_blocked)?
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

    fn is_blocked(&self, server_name: &ServerName, media_id: &str) -> Result<bool> {
        let blocked_via_hash = || {
            let mut key = server_name.as_bytes().to_vec();
            key.push(0xff);
            key.extend_from_slice(media_id.as_bytes());

            let Some(metadata) = self.servernamemediaid_metadata.get(&key)? else {
                return Ok(false);
            };

            let sha256_digest = parse_metadata(&metadata).inspect_err(|e| {
                error!("Error parsing metadata for \"mxc://{server_name}/{media_id}\" from servernamemediaid_metadata: {e}");
            })?.sha256_digest;

            self.is_blocked_filehash(&sha256_digest)
        };

        Ok(self.is_directly_blocked(server_name, media_id)? || blocked_via_hash()?)
    }

    fn block(
        &self,
        media: &[(OwnedServerName, String)],
        unix_secs: u64,
        reason: Option<String>,
    ) -> Vec<Error> {
        let reason = reason.unwrap_or_default();
        let unix_secs = unix_secs.to_be_bytes();

        let mut errors = Vec::new();

        for (server_name, media_id) in media {
            let mut key = server_name.as_bytes().to_vec();
            key.push(0xff);
            key.extend_from_slice(media_id.as_bytes());

            let mut value = unix_secs.to_vec();
            value.extend_from_slice(reason.as_bytes());

            if let Err(e) = self.blocked_servername_mediaid.insert(&key, &value) {
                errors.push(e);
            }
        }

        errors
    }

    fn block_from_user(
        &self,
        user_id: &UserId,
        now: u64,
        reason: &str,
        after: Option<u64>,
    ) -> Vec<Error> {
        let mut prefix = user_id.server_name().as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(user_id.localpart().as_bytes());
        prefix.push(0xff);

        let mut value = now.to_be_bytes().to_vec();
        value.extend_from_slice(reason.as_bytes());

        self.servername_userlocalpart_mediaid
            .scan_prefix(prefix)
            .map(|(k, _)| {
                let parts = k.split(|&b| b == 0xff);

                let media_id = parts.last().ok_or_else(|| {
                    Error::bad_database("Invalid format of key in blocked_servername_mediaid")
                })?;

                let mut key = user_id.server_name().as_bytes().to_vec();
                key.push(0xff);
                key.extend_from_slice(media_id);

                let Some(mut meta) = self.servernamemediaid_metadata.get(&key)? else {
                    return Err(Error::bad_database(
                        "Invalid format of metadata in servernamemediaid_metadata",
                    ));
                };
                meta.truncate(32);
                let sha256_digest = meta;

                let Some(metadata) = self
                    .filehash_metadata
                    .get(&sha256_digest)?
                    .map(FilehashMetadata::from_vec)
                else {
                    return Ok(());
                };

                if after
                    .map(|after| Ok::<bool, Error>(metadata.creation(&sha256_digest)? > after))
                    .transpose()?
                    .unwrap_or(true)
                {
                    self.blocked_servername_mediaid.insert(&key, &value)
                } else {
                    Ok(())
                }
            })
            .filter_map(Result::err)
            .collect()
    }

    fn unblock(&self, media: &[(OwnedServerName, String)]) -> Vec<Error> {
        let maybe_remove_remaining_metadata = |metadata: &DbFileMeta, errors: &mut Vec<Error>| {
            for (k, _) in self
                .filehash_servername_mediaid
                .scan_prefix(metadata.sha256_digest.clone())
            {
                if let Some(servername_mediaid) = k.get(32..) {
                    if let Err(e) = self.blocked_servername_mediaid.remove(servername_mediaid) {
                        errors.push(e);
                    }
                } else {
                    error!(
                    "Invalid format of key in filehash_servername_mediaid for media with sha256 content hash of {}",
                    hex::encode(&metadata.sha256_digest)
                );
                    errors.push(Error::BadDatabase(
                        "Invalid format of key in filehash_servername_mediaid",
                    ));
                }
            }

            let thumbnail_id_error = || {
                error!(
                "Invalid format of key in filehash_thumbnail_id for media with sha256 content hash of {}",
                hex::encode(&metadata.sha256_digest)
            );
                Error::BadDatabase("Invalid format of value in filehash_thumbnailid")
            };

            for (k, _) in self
                .filehash_thumbnailid
                .scan_prefix(metadata.sha256_digest.clone())
            {
                if let Some(end) = k.len().checked_sub(9) {
                    if let Some(servername_mediaid) = k.get(32..end) {
                        if let Err(e) = self.blocked_servername_mediaid.remove(servername_mediaid) {
                            errors.push(e);
                        }
                    } else {
                        errors.push(thumbnail_id_error());
                    }
                    errors.push(thumbnail_id_error());
                };
            }

            // If we don't have the actual file downloaded anymore, remove the remaining
            // metadata of the file
            match self
                .filehash_metadata
                .get(&metadata.sha256_digest)
                .map(|opt| opt.is_none())
            {
                Err(e) => errors.push(e),
                Ok(true) => {
                    if let Err(e) = self.purge_filehash(metadata.sha256_digest.clone(), false) {
                        errors.push(e);
                    }
                }
                Ok(false) => (),
            }
        };

        let mut errors = Vec::new();

        for (server_name, media_id) in media {
            let mut key = server_name.as_bytes().to_vec();
            key.push(0xff);
            key.extend_from_slice(media_id.as_bytes());

            match self
                .servernamemediaid_metadata
                .get(&key)
                .map(|opt| opt.as_deref().map(parse_metadata))
            {
                Err(e) => {
                    errors.push(e);
                    continue;
                }
                Ok(None) => (),
                Ok(Some(Err(e))) => {
                    error!("Error parsing metadata for \"mxc://{server_name}/{media_id}\" from servernamemediaid_metadata: {e}");
                    errors.push(e);
                    continue;
                }
                Ok(Some(Ok(metadata))) => {
                    maybe_remove_remaining_metadata(&metadata, &mut errors);
                }
            }

            key.push(0xff);
            for (_, v) in self.thumbnailid_metadata.scan_prefix(key) {
                match parse_metadata(&v) {
                    Ok(metadata) => {
                        maybe_remove_remaining_metadata(&metadata, &mut errors);
                    }
                    Err(e) => {
                        error!("Error parsing metadata for thumbnail of \"mxc://{server_name}/{media_id}\" from thumbnailid_metadata: {e}");
                        errors.push(e);
                    }
                }
            }
        }

        errors
    }

    fn list_blocked(&self) -> Vec<Result<BlockedMediaInfo>> {
        let parse_servername = |parts: &mut Split<_, _>| {
            OwnedServerName::try_from(
                utils::string_from_bytes(parts.next().ok_or_else(|| {
                    Error::BadDatabase("Invalid format of metadata of blocked media")
                })?)
                .map_err(|_| Error::BadDatabase("Invalid server_name String of blocked data"))?,
            )
            .map_err(|_| Error::BadDatabase("Invalid ServerName in blocked_servername_mediaid"))
        };

        let parse_string =
            |parts: &mut Split<_, _>| {
                utils::string_from_bytes(parts.next().ok_or_else(|| {
                    Error::BadDatabase("Invalid format of metadata of blocked media")
                })?)
                .map_err(|_| Error::BadDatabase("Invalid string in blocked media metadata"))
            };

        let splitter = |b: &u8| *b == 0xff;

        self.blocked_servername_mediaid
            .iter()
            .map(|(k, v)| {
                let mut parts = k.split(splitter);

                // Using map_err, as inspect_err causes lifetime issues
                // "implementation of `FnOnce` is not general enough"
                let log_error = |e| {
                    error!("Error parsing key of blocked media: {e}");
                    e
                };

                let server_name = parse_servername(&mut parts).map_err(log_error)?;

                let media_id = parse_string(&mut parts).map_err(log_error)?;

                let (unix_secs, reason) = v
                    .split_at_checked(8)
                    .map(|(secs, reason)| -> Result<(u64, Option<String>)> {
                        Ok((
                            secs.try_into()
                                .map_err(|_| {
                                    Error::bad_database(
                                        "Invalid block time in blocked_servername_mediaid ",
                                    )
                                })
                                .map(u64::from_be_bytes)?,
                            if reason.is_empty() {
                                None
                            } else {
                                Some(utils::string_from_bytes(reason).map_err(|_| {
                                    Error::bad_database("Invalid string in blocked media metadata")
                                })?)
                            },
                        ))
                    })
                    .ok_or_else(|| {
                        Error::bad_database("Invalid format of value in blocked_servername_mediaid")
                    })??;

                let sha256_hex = self.servernamemediaid_metadata.get(&k)?.map(|mut meta| {
                    meta.truncate(32);
                    hex::encode(meta)
                });

                Ok(BlockedMediaInfo {
                    server_name,
                    media_id,
                    unix_secs,
                    reason,
                    sha256_hex,
                })
            })
            .collect()
    }

    fn is_blocked_filehash(&self, sha256_digest: &[u8]) -> Result<bool> {
        for (filehash_servername_mediaid, _) in self
            .filehash_servername_mediaid
            .scan_prefix(sha256_digest.to_owned())
        {
            let servername_mediaid = filehash_servername_mediaid.get(32..).ok_or_else(|| {
                error!(
                    "Invalid format of key in filehash_servername_mediaid for media with sha256 content hash of {}",
                    hex::encode(sha256_digest)
                );
                Error::BadDatabase("Invalid format of key in filehash_servername_mediaid")
            })?;

            if self
                .blocked_servername_mediaid
                .get(servername_mediaid)?
                .is_some()
            {
                return Ok(true);
            }
        }

        let thumbnail_id_error = || {
            error!(
                "Invalid format of key in filehash_thumbnail_id for media with sha256 content hash of {}",
                hex::encode(sha256_digest)
            );
            Error::BadDatabase("Invalid format of value in filehash_thumbnailid")
        };

        for (thumbnail_id, _) in self
            .filehash_thumbnailid
            .scan_prefix(sha256_digest.to_owned())
        {
            let servername_mediaid = thumbnail_id
                .get(
                    32..thumbnail_id
                        .len()
                        .checked_sub(9)
                        .ok_or_else(thumbnail_id_error)?,
                )
                .ok_or_else(thumbnail_id_error)?;

            if self
                .blocked_servername_mediaid
                .get(servername_mediaid)?
                .is_some()
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn files_to_delete(
        &self,
        sha256_digest: &[u8],
        retention: &MediaRetentionConfig,
        media_type: MediaType,
        new_size: u64,
    ) -> Result<Vec<Result<String>>> {
        // If the file already exists, no space needs to be cleared
        if self.filehash_metadata.get(sha256_digest)?.is_some() {
            return Ok(Vec::new());
        }

        let scoped_space = |scope| retention.scoped.get(&scope).and_then(|policy| policy.space);

        let mut files_to_delete = Vec::new();

        if media_type.is_thumb() {
            if let Some(mut f) = self.purge_if_necessary(
                scoped_space(MediaRetentionScope::Thumbnail),
                |k| self.file_is_thumb(k),
                &new_size,
            ) {
                files_to_delete.append(&mut f);
            }
        }

        match media_type {
            MediaType::LocalMedia { thumbnail: _ } => {
                if let Some(mut f) = self.purge_if_necessary(
                    scoped_space(MediaRetentionScope::Local),
                    |k| self.file_is_local(k).unwrap_or(true),
                    &new_size,
                ) {
                    files_to_delete.append(&mut f);
                }
            }
            MediaType::RemoteMedia { thumbnail: _ } => {
                if let Some(mut f) = self.purge_if_necessary(
                    scoped_space(MediaRetentionScope::Remote),
                    |k| !self.file_is_local(k).unwrap_or(true),
                    &new_size,
                ) {
                    files_to_delete.append(&mut f);
                }
            }
        }

        if let Some(mut f) = self.purge_if_necessary(retention.global_space, |_| true, &new_size) {
            files_to_delete.append(&mut f);
        }

        Ok(files_to_delete)
    }

    fn cleanup_time_retention(&self, retention: &MediaRetentionConfig) -> Vec<Result<String>> {
        let now = utils::secs_since_unix_epoch();

        let should_be_deleted = |k: &[u8], metadata: &FilehashMetadata| {
            let check_policy = |retention_scope| {
                if let Some(scoped_retention) = retention.scoped.get(&retention_scope) {
                    if let Some(created_policy) = scoped_retention.created {
                        if now - metadata.creation(k)? > created_policy.as_secs() {
                            return Ok(true);
                        }
                    }

                    if let Some(accessed_policy) = scoped_retention.accessed {
                        if now - metadata.last_access(k)? > accessed_policy.as_secs() {
                            return Ok(true);
                        }
                    }
                }
                Ok(false)
            };

            if self.file_is_thumb(k) && check_policy(MediaRetentionScope::Thumbnail)? {
                return Ok(true);
            }

            if self.file_is_local(k)? {
                check_policy(MediaRetentionScope::Local)
            } else {
                check_policy(MediaRetentionScope::Remote)
            }
        };

        let mut files_to_delete = Vec::new();
        let mut errors_and_hashes = Vec::new();

        for (k, v) in self.filehash_metadata.iter() {
            match should_be_deleted(&k, &FilehashMetadata::from_vec(v)) {
                Ok(true) => files_to_delete.push(k),
                Ok(false) => (),
                Err(e) => errors_and_hashes.push(Err(e)),
            }
        }

        errors_and_hashes.append(&mut self.purge(files_to_delete));

        errors_and_hashes
    }

    fn update_last_accessed(&self, server_name: &ServerName, media_id: &str) -> Result<()> {
        let mut key = server_name.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(media_id.as_bytes());

        if let Some(mut meta) = self.servernamemediaid_metadata.get(&key)? {
            meta.truncate(32);
            let sha256_digest = meta;

            self.update_last_accessed_filehash(&sha256_digest)
        } else {
            // File was probably deleted just as we were fetching it, so nothing to do
            Ok(())
        }
    }

    fn update_last_accessed_filehash(&self, sha256_digest: &[u8]) -> Result<()> {
        if let Some(mut metadata) = self
            .filehash_metadata
            .get(sha256_digest)?
            .map(FilehashMetadata::from_vec)
        {
            metadata.update_last_access();

            self.filehash_metadata
                .insert(sha256_digest, metadata.value())
        } else {
            // File was probably deleted just as we were fetching it, so nothing to do
            Ok(())
        }
    }
}

impl KeyValueDatabase {
    /// Only checks whether the media id itself is blocked, and not associated filehashes
    fn is_directly_blocked(&self, server_name: &ServerName, media_id: &str) -> Result<bool> {
        let mut key = server_name.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(media_id.as_bytes());

        self.blocked_servername_mediaid
            .get(&key)
            .map(|x| x.is_some())
    }

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

    fn file_is_local(&self, k: &[u8]) -> Result<bool> {
        for (k, _) in self.filehash_servername_mediaid.scan_prefix(k.to_vec()) {
            let mut parts = k
                .get(32..)
                .map(|k| k.split(|&b| b == 0xff))
                .ok_or_else(|| {
                    Error::bad_database("Invalid format of key in filehash_servername_mediaid")
                })?;

            let Some(server_name) = parts.next() else {
                return Err(Error::bad_database(
                    "Invalid format of key in filehash_servername_mediaid",
                ));
            };

            if utils::string_from_bytes(server_name).map_err(|_| {
                Error::bad_database("Invalid UTF-8 servername in filehash_servername_mediaid")
            })? == services().globals.server_name().as_str()
            {
                return Ok(true);
            }
        }

        Ok(false)
    }

    fn file_is_thumb(&self, k: &[u8]) -> bool {
        self.filehash_thumbnailid
            .scan_prefix(k.to_vec())
            .next()
            .is_some()
            && self
                .filehash_servername_mediaid
                .scan_prefix(k.to_vec())
                .next()
                .is_none()
    }

    fn purge_if_necessary(
        &self,
        space: Option<ByteSize>,
        filter: impl Fn(&[u8]) -> bool,
        new_size: &u64,
    ) -> Option<Vec<Result<String>>> {
        if let Some(space) = space {
            let mut candidate_files_to_delete = Vec::new();
            let mut errors_and_hashes = Vec::new();
            let mut total_size = 0;

            let parse_value = |k: Vec<u8>, v: &FilehashMetadata| {
                let last_access = v.last_access(&k)?;
                let size = v.size(&k)?;
                Ok((k, last_access, size))
            };

            for (k, v) in self.filehash_metadata.iter().filter(|(k, _)| filter(k)) {
                match parse_value(k, &FilehashMetadata::from_vec(v)) {
                    Ok(x) => {
                        total_size += x.2;
                        candidate_files_to_delete.push(x)
                    }
                    Err(e) => errors_and_hashes.push(Err(e)),
                }
            }

            if let Some(required_to_delete) = (total_size + *new_size).checked_sub(space.as_u64()) {
                candidate_files_to_delete.sort_by_key(|(_, last_access, _)| *last_access);
                candidate_files_to_delete.reverse();

                let mut size_sum = 0;
                let mut take = candidate_files_to_delete.len();

                for (i, (_, _, file_size)) in candidate_files_to_delete.iter().enumerate() {
                    size_sum += file_size;
                    if size_sum >= required_to_delete {
                        take = i + 1;
                        break;
                    }
                }

                errors_and_hashes.append(
                    &mut self.purge(
                        candidate_files_to_delete
                            .into_iter()
                            .take(take)
                            .map(|(hash, _, _)| hash)
                            .collect(),
                    ),
                );

                Some(errors_and_hashes)
            } else {
                None
            }
        } else {
            None
        }
    }

    fn purge(&self, hashes: Vec<Vec<u8>>) -> Vec<Result<String>> {
        hashes
            .into_iter()
            .map(|sha256_digest| {
                let sha256_hex = hex::encode(&sha256_digest);
                let is_blocked = self.is_blocked_filehash(&sha256_digest)?;

                self.purge_filehash(sha256_digest, is_blocked)?;

                Ok(sha256_hex)
            })
            .collect()
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

    pub fn update_last_access(&mut self) {
        let now = utils::secs_since_unix_epoch().to_be_bytes();
        self.value.truncate(16);
        self.value.extend_from_slice(&now);
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

    pub fn size(&self, sha256_digest: &[u8]) -> Result<u64> {
        self.get_u64_val(
            0..8,
            "file size",
            sha256_digest,
            "Invalid file size in filehash_metadata",
        )
    }

    pub fn creation(&self, sha256_digest: &[u8]) -> Result<u64> {
        self.get_u64_val(
            8..16,
            "creation time",
            sha256_digest,
            "Invalid creation time in filehash_metadata",
        )
    }

    pub fn last_access(&self, sha256_digest: &[u8]) -> Result<u64> {
        self.get_u64_val(
            16..24,
            "last access time",
            sha256_digest,
            "Invalid last access time in filehash_metadata",
        )
    }
}
