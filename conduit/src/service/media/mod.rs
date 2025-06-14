mod data;
use std::{io::Cursor, sync::Arc};

pub use data::Data;
use http::StatusCode;
use ruma::{
    OwnedServerName, ServerName, UserId,
    api::client::{error::ErrorKind, media::is_safe_inline_content_type},
    http_headers::{ContentDisposition, ContentDispositionType},
};
use rusty_s3::{
    S3Action,
    actions::{DeleteObjectsResponse, ObjectIdentifier},
};
use sha2::{Digest, Sha256, digest::Output};
use tracing::{error, info, warn};

use crate::{
    Error, Result,
    config::{DirectoryStructure, MediaBackendConfig, S3MediaBackend},
    service::rate_limiting::Target,
    services, utils,
};
use image::imageops::FilterType;

pub struct DbFileMeta {
    pub sha256_digest: Vec<u8>,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub unauthenticated_access_permitted: bool,
}

use tokio::{
    fs::{self, File},
    io::{AsyncReadExt, AsyncWriteExt},
};

pub struct MediaQuery {
    pub is_blocked: bool,
    pub source_file: Option<MediaQueryFileInfo>,
    pub thumbnails: Vec<MediaQueryThumbInfo>,
}

pub struct MediaQueryFileInfo {
    pub uploader_localpart: Option<String>,
    pub sha256_hex: String,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub unauthenticated_access_permitted: bool,
    pub is_blocked_via_filehash: bool,
    pub file_info: Option<FileInfo>,
}

pub struct MediaQueryThumbInfo {
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

pub struct MediaListItem {
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
    ServerName(Box<ServerName>),
    UserId(Box<UserId>),
}

pub struct FileMeta {
    pub content_disposition: ContentDisposition,
    pub content_type: Option<String>,
    pub file: Vec<u8>,
}

pub enum MediaType {
    LocalMedia { thumbnail: bool },
    RemoteMedia { thumbnail: bool },
}

impl MediaType {
    pub fn new(server_name: &ServerName, thumbnail: bool) -> Self {
        if server_name == services().globals.server_name() {
            Self::LocalMedia { thumbnail }
        } else {
            Self::RemoteMedia { thumbnail }
        }
    }

    pub fn is_thumb(&self) -> bool {
        match self {
            MediaType::LocalMedia { thumbnail } | MediaType::RemoteMedia { thumbnail } => {
                *thumbnail
            }
        }
    }
}

pub struct Service {
    pub db: &'static dyn Data,
}

pub struct BlockedMediaInfo {
    pub server_name: OwnedServerName,
    pub media_id: String,
    pub unix_secs: u64,
    pub reason: Option<String>,
    pub sha256_hex: Option<String>,
}

impl Service {
    pub fn start_time_retention_checker(self: &Arc<Self>) {
        let self2 = Arc::clone(self);
        if let Some(cleanup_interval) = services().globals.config.media.retention.cleanup_interval()
        {
            tokio::spawn(async move {
                let mut i = cleanup_interval;
                loop {
                    i.tick().await;
                    let _ = self2.try_purge_time_retention().await;
                }
            });
        }
    }

    async fn try_purge_time_retention(&self) -> Result<()> {
        info!("Checking if any media should be deleted due to time-based retention policies");
        let files = self
            .db
            .cleanup_time_retention(&services().globals.config.media.retention);

        let count = files.iter().filter(|res| res.is_ok()).count();
        info!("Found {count} media files to delete");

        purge_files(files).await;

        Ok(())
    }

    /// Uploads a file.
    pub async fn create(
        &self,
        servername: &ServerName,
        media_id: &str,
        filename: Option<&str>,
        content_type: Option<&str>,
        file: &[u8],
        user_id: Option<&UserId>,
    ) -> Result<()> {
        let (sha256_digest, sha256_hex) = generate_digests(file);

        for error in self
            .clear_required_space(
                &sha256_digest,
                MediaType::new(servername, false),
                size(file)?,
            )
            .await?
        {
            error!(
                "Error deleting file to clear space when downloading/creating new media file: {error}"
            )
        }

        self.db.create_file_metadata(
            sha256_digest,
            size(file)?,
            servername,
            media_id,
            filename,
            content_type,
            user_id,
            self.db.is_blocked_filehash(&sha256_digest)?,
        )?;

        if !self.db.is_blocked_filehash(&sha256_digest)? {
            create_file(&sha256_hex, file).await
        } else if user_id.is_none() {
            Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
        } else {
            Ok(())
        }
    }

    /// Uploads or replaces a file thumbnail.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_thumbnail(
        &self,
        servername: &ServerName,
        media_id: &str,
        filename: Option<&str>,
        content_type: Option<&str>,
        width: u32,
        height: u32,
        file: &[u8],
    ) -> Result<()> {
        let (sha256_digest, sha256_hex) = generate_digests(file);

        self.clear_required_space(
            &sha256_digest,
            MediaType::new(servername, true),
            size(file)?,
        )
        .await?;

        self.db.create_thumbnail_metadata(
            sha256_digest,
            size(file)?,
            servername,
            media_id,
            width,
            height,
            filename,
            content_type,
        )?;

        create_file(&sha256_hex, file).await
    }

    /// Fetches a local file and it's metadata
    pub async fn get(
        &self,
        servername: &ServerName,
        media_id: &str,
        target: Option<Target>,
    ) -> Result<Option<FileMeta>> {
        let DbFileMeta {
            sha256_digest,
            filename,
            content_type,
            unauthenticated_access_permitted,
        } = self.db.search_file_metadata(servername, media_id)?;

        if !(target.as_ref().is_some_and(Target::is_authenticated)
            || unauthenticated_access_permitted)
        {
            return Ok(None);
        }

        let file = self.get_file(&sha256_digest, None).await?;

        services()
            .rate_limiting
            .check_media_download(target, size(&file)?)
            .await?;

        Ok(Some(FileMeta {
            content_disposition: content_disposition(filename, &content_type),
            content_type,
            file,
        }))
    }

    /// Returns width, height of the thumbnail and whether it should be cropped. Returns None when
    /// the server should send the original file.
    pub fn thumbnail_properties(&self, width: u32, height: u32) -> Option<(u32, u32, bool)> {
        match (width, height) {
            (0..=32, 0..=32) => Some((32, 32, true)),
            (0..=96, 0..=96) => Some((96, 96, true)),
            (0..=320, 0..=240) => Some((320, 240, false)),
            (0..=640, 0..=480) => Some((640, 480, false)),
            (0..=800, 0..=600) => Some((800, 600, false)),
            _ => None,
        }
    }

    /// Downloads a file's thumbnail.
    ///
    /// Here's an example on how it works:
    ///
    /// - Client requests an image with width=567, height=567
    /// - Server rounds that up to (800, 600), so it doesn't have to save too many thumbnails
    /// - Server rounds that up again to (958, 600) to fix the aspect ratio (only for width,height>96)
    /// - Server creates the thumbnail and sends it to the user
    ///
    /// For width,height <= 96 the server uses another thumbnailing algorithm which crops the image afterwards.
    pub async fn get_thumbnail(
        &self,
        servername: &ServerName,
        media_id: &str,
        width: u32,
        height: u32,
        target: Option<Target>,
    ) -> Result<Option<FileMeta>> {
        if let Some((width, height, crop)) = self.thumbnail_properties(width, height) {
            if let Ok(DbFileMeta {
                sha256_digest,
                filename,
                content_type,
                unauthenticated_access_permitted,
            }) = self
                .db
                .search_thumbnail_metadata(servername, media_id, width, height)
            {
                if !(target.as_ref().is_some_and(Target::is_authenticated)
                    || unauthenticated_access_permitted)
                {
                    return Ok(None);
                }

                let file_info = self.file_info(&sha256_digest)?;

                services()
                    .rate_limiting
                    .check_media_download(target, file_info.size)
                    .await?;

                // Using saved thumbnail
                let file = self
                    .get_file(&sha256_digest, Some((servername, media_id)))
                    .await?;

                Ok(Some(FileMeta {
                    content_disposition: content_disposition(filename, &content_type),
                    content_type,
                    file,
                }))
            } else if !target.as_ref().is_some_and(Target::is_authenticated) {
                return Ok(None);
            } else if let Ok(DbFileMeta {
                sha256_digest,
                filename,
                content_type,
                ..
            }) = self.db.search_file_metadata(servername, media_id)
            {
                let content_disposition = content_disposition(filename.clone(), &content_type);
                // Generate a thumbnail
                let file = self.get_file(&sha256_digest, None).await?;

                if let Ok(image) = image::load_from_memory(&file) {
                    let original_width = image.width();
                    let original_height = image.height();
                    if width > original_width || height > original_height {
                        return Ok(Some(FileMeta {
                            content_disposition,
                            content_type,
                            file,
                        }));
                    }

                    let thumbnail = if crop {
                        image.resize_to_fill(width, height, FilterType::CatmullRom)
                    } else {
                        let (exact_width, exact_height) = {
                            // Copied from image::dynimage::resize_dimensions
                            let ratio = u64::from(original_width) * u64::from(height);
                            let nratio = u64::from(width) * u64::from(original_height);

                            let use_width = nratio <= ratio;
                            let intermediate = if use_width {
                                u64::from(original_height) * u64::from(width)
                                    / u64::from(original_width)
                            } else {
                                u64::from(original_width) * u64::from(height)
                                    / u64::from(original_height)
                            };
                            if use_width {
                                if intermediate <= u64::from(u32::MAX) {
                                    (width, intermediate as u32)
                                } else {
                                    (
                                        (u64::from(width) * u64::from(u32::MAX) / intermediate)
                                            as u32,
                                        u32::MAX,
                                    )
                                }
                            } else if intermediate <= u64::from(u32::MAX) {
                                (intermediate as u32, height)
                            } else {
                                (
                                    u32::MAX,
                                    (u64::from(height) * u64::from(u32::MAX) / intermediate) as u32,
                                )
                            }
                        };

                        image.thumbnail_exact(exact_width, exact_height)
                    };

                    let mut thumbnail_bytes = Vec::new();
                    thumbnail.write_to(
                        &mut Cursor::new(&mut thumbnail_bytes),
                        image::ImageFormat::Png,
                    )?;

                    // Save thumbnail in database so we don't have to generate it again next time
                    self.upload_thumbnail(
                        servername,
                        media_id,
                        filename.as_deref(),
                        content_type.as_deref(),
                        width,
                        height,
                        &thumbnail_bytes,
                    )
                    .await?;

                    Ok(Some(FileMeta {
                        content_disposition,
                        content_type,
                        file: thumbnail_bytes,
                    }))
                } else {
                    // Couldn't parse file to generate thumbnail, likely not an image
                    Err(Error::BadRequest(
                        ErrorKind::Unknown,
                        "Unable to generate thumbnail for the requested content (likely is not an image)",
                    ))
                }
            } else {
                Ok(None)
            }
        } else {
            // Using full-sized file
            let Ok(DbFileMeta {
                sha256_digest,
                filename,
                content_type,
                unauthenticated_access_permitted,
            }) = self.db.search_file_metadata(servername, media_id)
            else {
                return Ok(None);
            };

            if !(target.as_ref().is_some_and(Target::is_authenticated)
                || unauthenticated_access_permitted)
            {
                return Ok(None);
            }

            let file = self.get_file(&sha256_digest, None).await?;

            Ok(Some(FileMeta {
                content_disposition: content_disposition(filename, &content_type),
                content_type,
                file,
            }))
        }
    }

    /// Returns information about the queried media
    pub fn query(&self, server_name: &ServerName, media_id: &str) -> Result<MediaQuery> {
        self.db.query(server_name, media_id)
    }

    /// Purges all of the specified media.
    ///
    /// If `force_filehash` is true, all media and/or thumbnails which share sha256 content hashes
    /// with the purged media will also be purged, meaning that the media is guaranteed to be deleted
    /// from the media backend. Otherwise, it will be deleted if only the media IDs requested to be
    /// purged have that sha256 hash.
    ///
    /// Returns errors for all the files that were failed to be deleted, if any.
    pub async fn purge(
        &self,
        media: &[(OwnedServerName, String)],
        force_filehash: bool,
    ) -> Vec<Error> {
        let hashes = self.db.purge_and_get_hashes(media, force_filehash);

        purge_files(hashes).await
    }

    /// Purges all (past a certain time in unix seconds, if specified) media
    /// sent by a user.
    ///
    /// If `force_filehash` is true, all media and/or thumbnails which share sha256 content hashes
    /// with the purged media will also be purged, meaning that the media is guaranteed to be deleted
    /// from the media backend. Otherwise, it will be deleted if only the media IDs requested to be
    /// purged have that sha256 hash.
    ///
    /// Returns errors for all the files that were failed to be deleted, if any.
    ///
    /// Note: it only currently works for local users, as we cannot determine who
    /// exactly uploaded the file when it comes to remove users.
    pub async fn purge_from_user(
        &self,
        user_id: &UserId,
        force_filehash: bool,
        after: Option<u64>,
    ) -> Vec<Error> {
        let hashes = self
            .db
            .purge_and_get_hashes_from_user(user_id, force_filehash, after);

        purge_files(hashes).await
    }

    /// Purges all (past a certain time in unix seconds, if specified) media
    /// obtained from the specified server (due to the MXC URI).
    ///
    /// If `force_filehash` is true, all media and/or thumbnails which share sha256 content hashes
    /// with the purged media will also be purged, meaning that the media is guaranteed to be deleted
    /// from the media backend. Otherwise, it will be deleted if only the media IDs requested to be
    /// purged have that sha256 hash.
    ///
    /// Returns errors for all the files that were failed to be deleted, if any.
    pub async fn purge_from_server(
        &self,
        server_name: &ServerName,
        force_filehash: bool,
        after: Option<u64>,
    ) -> Vec<Error> {
        let hashes = self
            .db
            .purge_and_get_hashes_from_server(server_name, force_filehash, after);

        purge_files(hashes).await
    }

    /// Checks whether the media has been blocked by administrators, returning either
    /// a database error, or a not found error if it is blocked
    pub fn check_blocked(&self, server_name: &ServerName, media_id: &str) -> Result<()> {
        if self.db.is_blocked(server_name, media_id)? {
            Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
        } else {
            Ok(())
        }
    }

    /// Marks the specified media as blocked, preventing them from being accessed
    pub fn block(&self, media: &[(OwnedServerName, String)], reason: Option<String>) -> Vec<Error> {
        let now = utils::secs_since_unix_epoch();

        self.db.block(media, now, reason)
    }

    /// Marks the media uploaded by a local user as blocked, preventing it from being accessed
    pub fn block_from_user(
        &self,
        user_id: &UserId,
        reason: &str,
        after: Option<u64>,
    ) -> Vec<Error> {
        let now = utils::secs_since_unix_epoch();

        self.db.block_from_user(user_id, now, reason, after)
    }

    /// Unblocks the specified media, allowing them from being accessed again
    pub fn unblock(&self, media: &[(OwnedServerName, String)]) -> Vec<Error> {
        self.db.unblock(media)
    }

    /// Returns a list of all the stored media, applying all the given filters to the results
    pub fn list(
        &self,
        server_name_or_user_id: Option<ServerNameOrUserId>,
        include_thumbnails: bool,
        content_type: Option<&str>,
        before: Option<u64>,
        after: Option<u64>,
    ) -> Result<Vec<MediaListItem>> {
        self.db.list(
            server_name_or_user_id,
            include_thumbnails,
            content_type,
            before,
            after,
        )
    }

    /// Returns a Vec of:
    /// - The server the media is from
    /// - The media id
    /// - The time it was blocked, in unix seconds
    /// - The optional reason why it was blocked
    pub fn list_blocked(&self) -> Vec<Result<BlockedMediaInfo>> {
        self.db.list_blocked()
    }

    pub async fn clear_required_space(
        &self,
        sha256_digest: &[u8],
        media_type: MediaType,
        new_size: u64,
    ) -> Result<Vec<Error>> {
        let files = self.db.files_to_delete(
            sha256_digest,
            &services().globals.config.media.retention,
            media_type,
            new_size,
        )?;

        let count = files.iter().filter(|r| r.is_ok()).count();

        if count != 0 {
            info!("Deleting {} files to clear space for new media file", count);
        }

        Ok(purge_files(files).await)
    }

    /// Fetches the file from the configured media backend, as well as updating the "last accessed"
    /// part of the metadata of the file
    ///
    /// If specified, the original file will also have it's last accessed time updated, if present
    /// (use when accessing thumbnails)
    async fn get_file(
        &self,
        sha256_digest: &[u8],
        original_file_id: Option<(&ServerName, &str)>,
    ) -> Result<Vec<u8>> {
        let file = match &services().globals.config.media.backend {
            MediaBackendConfig::FileSystem {
                path,
                directory_structure,
            } => {
                let path = services().globals.get_media_path(
                    path,
                    directory_structure,
                    &hex::encode(sha256_digest),
                )?;

                let mut file = Vec::new();
                File::open(path).await?.read_to_end(&mut file).await?;

                file
            }
            MediaBackendConfig::S3(s3) => {
                let sha256_hex = hex::encode(sha256_digest);
                let file_name = services()
                    .globals
                    .split_media_path(s3.path.as_deref(), &s3.directory_structure, &sha256_hex)
                    .join("/");
                let url = s3
                    .bucket
                    .get_object(Some(&s3.credentials), &file_name)
                    .sign(s3.duration);

                let client = services().globals.default_client();
                let resp = client.get(url).send().await?;

                if resp.status() == StatusCode::NOT_FOUND {
                    return Err(Error::BadRequest(
                        ErrorKind::NotFound,
                        "File does not exist",
                    ));
                }
                if !resp.status().is_success() {
                    error!(
                        "Failed to get file with sha256 hash of \"{}\" from S3 bucket: {}",
                        sha256_hex,
                        resp.text().await?
                    );
                    return Err(Error::BadS3Response(
                        "Failed to get media file from S3 bucket",
                    ));
                }

                resp.bytes().await?.to_vec()
            }
        };

        if let Some((server_name, media_id)) = original_file_id {
            self.db.update_last_accessed(server_name, media_id)?;
        }

        self.db
            .update_last_accessed_filehash(sha256_digest)
            .map(|_| file)
    }

    fn file_info(&self, sha256_digest: &[u8]) -> Result<FileInfo> {
        self.db
            .file_info(sha256_digest)
            .transpose()
            .unwrap_or_else(|| Err(Error::BadRequest(ErrorKind::NotFound, "Fi)le not found")))
    }
}

/// Creates the media file, using the configured media backend
///
/// Note: this function does NOT set the metadata related to the file
pub async fn create_file(sha256_hex: &str, file: &[u8]) -> Result<()> {
    match &services().globals.config.media.backend {
        MediaBackendConfig::FileSystem {
            path,
            directory_structure,
        } => {
            let path = services()
                .globals
                .get_media_path(path, directory_structure, sha256_hex)?;

            // Create all directories leading up to file
            if let DirectoryStructure::Deep { .. } = directory_structure {
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(&parent).await.inspect_err(|e| error!("Error creating leading directories for media with sha256 hash of {sha256_hex}: {e}"))?;
                }
            }

            let mut f = File::create(path).await?;
            f.write_all(file).await?;
        }
        MediaBackendConfig::S3(s3) => {
            let file_name = services()
                .globals
                .split_media_path(s3.path.as_deref(), &s3.directory_structure, sha256_hex)
                .join("/");

            let url = s3
                .bucket
                .put_object(Some(&s3.credentials), &file_name)
                .sign(s3.duration);

            let client = services().globals.default_client();
            let resp = client.put(url).body(file.to_vec()).send().await?;

            if !resp.status().is_success() {
                error!(
                    "Failed to upload file with sha256 hash of \"{}\" to S3 bucket: {}",
                    sha256_hex,
                    resp.text().await?
                );
                return Err(Error::BadS3Response(
                    "Failed to upload media file to S3 bucket",
                ));
            }
        }
    }

    Ok(())
}

/// The size of a chunk for S3 delete operation.
const S3_CHUNK_SIZE: usize = 1000;

/// Purges the given files from the media backend
/// Returns a `Vec` of errors that occurred when attempting to delete the files
///
/// Note: this does NOT remove the related metadata from the database
async fn purge_files(hashes: Vec<Result<String>>) -> Vec<Error> {
    let (ok_values, err_values): (Vec<_>, Vec<_>) =
        hashes.into_iter().partition(|result| result.is_ok());

    let mut result: Vec<Error> = err_values.into_iter().map(Result::unwrap_err).collect();

    let to_delete: Vec<String> = ok_values.into_iter().map(Result::unwrap).collect();

    match &services().globals.config.media.backend {
        MediaBackendConfig::FileSystem {
            path,
            directory_structure,
        } => {
            for v in to_delete {
                if let Err(err) = delete_file_fs(path, directory_structure, &v).await {
                    result.push(err);
                }
            }
        }
        MediaBackendConfig::S3(s3) => {
            for chunk in to_delete.chunks(S3_CHUNK_SIZE) {
                match delete_files_s3(s3, chunk).await {
                    Ok(errors) => {
                        result.extend(errors);
                    }
                    Err(error) => {
                        result.push(error);
                    }
                }
            }
        }
    }

    result
}

/// Deletes the given file from the fs media backend
///
/// Note: this does NOT remove the related metadata from the database
async fn delete_file_fs(
    path: &str,
    directory_structure: &DirectoryStructure,
    sha256_hex: &str,
) -> Result<()> {
    let mut path = services()
        .globals
        .get_media_path(path, directory_structure, sha256_hex)?;

    if let Err(e) = fs::remove_file(&path).await {
        // Multiple files with the same filehash might be requseted to be deleted
        if e.kind() != std::io::ErrorKind::NotFound {
            error!("Error removing media from filesystem: {e}");
            Err(e)?;
        }
    }

    if let DirectoryStructure::Deep { length: _, depth } = directory_structure {
        let mut depth = depth.get();

        while depth > 0 {
            // Here at the start so that the first time, the file gets removed from the path
            path.pop();

            if let Err(e) = fs::remove_dir(&path).await {
                if e.kind() == std::io::ErrorKind::DirectoryNotEmpty {
                    break;
                } else {
                    error!("Error removing empty media directories: {e}");
                    Err(e)?;
                }
            }

            depth -= 1;
        }
    }

    Ok(())
}

/// Deletes the given files from the s3 media backend
///
/// Note: this does NOT remove the related metadata from the database
async fn delete_files_s3(s3: &S3MediaBackend, files: &[String]) -> Result<Vec<Error>> {
    let objects: Vec<ObjectIdentifier> = files
        .iter()
        .map(|v| {
            services()
                .globals
                .split_media_path(s3.path.as_deref(), &s3.directory_structure, v)
                .join("/")
        })
        .map(|v| ObjectIdentifier::new(v.to_string()))
        .collect();

    let mut request = s3
        .bucket
        .delete_objects(Some(&s3.credentials), objects.iter());
    request.set_quiet(true);

    let url = request.sign(s3.duration);
    let (body, md5) = request.body_with_md5();

    let client = services().globals.default_client();
    let resp = client
        .post(url)
        .header("Content-MD5", md5)
        .body(body)
        .send()
        .await?;

    if !resp.status().is_success() {
        error!(
            "Failed to delete files from S3 bucket: {}",
            resp.text().await?
        );
        return Err(Error::BadS3Response(
            "Failed to delete media files from S3 bucket",
        ));
    }

    let parsed = DeleteObjectsResponse::parse(resp.text().await?).map_err(|e| {
        warn!("Cannot parse S3 response: {}", e);
        Error::BadS3Response("Cannot parse S3 response")
    })?;

    let result = parsed
        .errors
        .into_iter()
        .map(|v| Error::CannotDeleteS3File(v.message))
        .collect();

    Ok(result)
}

/// Creates a content disposition with the given `filename`, using the `content_type` to determine whether
/// the disposition should be `inline` or `attachment`
fn content_disposition(
    filename: Option<String>,
    content_type: &Option<String>,
) -> ContentDisposition {
    ContentDisposition::new(
        if content_type
            .as_deref()
            .is_some_and(is_safe_inline_content_type)
        {
            ContentDispositionType::Inline
        } else {
            ContentDispositionType::Attachment
        },
    )
    .with_filename(filename)
}

/// Returns sha256 digests of the file, in raw (Vec) and hex form respectively
fn generate_digests(file: &[u8]) -> (Output<Sha256>, String) {
    let sha256_digest = Sha256::digest(file);
    let hex_sha256 = hex::encode(sha256_digest);

    (sha256_digest, hex_sha256)
}

/// Get's the file size, is bytes, as u64, returning an error if the file size is larger
/// than a u64 (which is far too big to be reasonably uploaded in the first place anyways)
pub fn size(file: &[u8]) -> Result<u64> {
    u64::try_from(file.len())
        .map_err(|_| Error::BadRequest(ErrorKind::TooLarge, "File is too large"))
}
