mod data;
use std::{fs, io::Cursor};

pub use data::Data;
use ruma::{
    api::client::{error::ErrorKind, media::is_safe_inline_content_type},
    http_headers::{ContentDisposition, ContentDispositionType},
    OwnedServerName, ServerName, UserId,
};
use sha2::{digest::Output, Digest, Sha256};
use tracing::error;

use crate::{
    config::{DirectoryStructure, MediaConfig},
    services, Error, Result,
};
use image::imageops::FilterType;

pub struct DbFileMeta {
    pub sha256_digest: Vec<u8>,
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub unauthenticated_access_permitted: bool,
}

use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};

pub struct FileMeta {
    pub content_disposition: ContentDisposition,
    pub content_type: Option<String>,
    pub file: Vec<u8>,
}

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
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

        self.db.create_file_metadata(
            sha256_digest,
            size(file)?,
            servername,
            media_id,
            filename,
            content_type,
            user_id,
        )?;

        create_file(&sha256_hex, file).await
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
        authenticated: bool,
    ) -> Result<Option<FileMeta>> {
        let DbFileMeta {
            sha256_digest,
            filename,
            content_type,
            unauthenticated_access_permitted,
        } = self.db.search_file_metadata(servername, media_id)?;

        if !(authenticated || unauthenticated_access_permitted) {
            return Ok(None);
        }

        let file = get_file(&hex::encode(sha256_digest)).await?;

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
        authenticated: bool,
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
                if !(authenticated || unauthenticated_access_permitted) {
                    return Ok(None);
                }

                // Using saved thumbnail
                let file = get_file(&hex::encode(sha256_digest)).await?;

                Ok(Some(FileMeta {
                    content_disposition: content_disposition(filename, &content_type),
                    content_type,
                    file,
                }))
            } else if !authenticated {
                return Ok(None);
            } else if let Ok(DbFileMeta {
                sha256_digest,
                filename,
                content_type,
                unauthenticated_access_permitted,
            }) = self.db.search_file_metadata(servername, media_id)
            {
                if !(authenticated || unauthenticated_access_permitted) {
                    return Ok(None);
                }

                let content_disposition = content_disposition(filename.clone(), &content_type);
                // Generate a thumbnail
                let file = get_file(&hex::encode(sha256_digest)).await?;

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

            if !(authenticated || unauthenticated_access_permitted) {
                return Ok(None);
            }

            let file = get_file(&hex::encode(sha256_digest)).await?;

            Ok(Some(FileMeta {
                content_disposition: content_disposition(filename, &content_type),
                content_type,
                file,
            }))
        }
    }

    /// Purges all of the specified media.
    ///
    /// If `force_filehash` is true, all media and/or thumbnails which share sha256 content hashes
    /// with the purged media will also be purged, meaning that the media is guaranteed to be deleted
    /// from the media backend. Otherwise, it will be deleted if only the media IDs requested to be
    /// purged have that sha256 hash.
    ///
    /// Returns errors for all the files that were failed to be deleted, if any.
    pub fn purge(&self, media: &[(OwnedServerName, String)], force_filehash: bool) -> Vec<Error> {
        let hashes = self.db.purge_and_get_hashes(media, force_filehash);

        purge_files(hashes)
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
    pub fn purge_from_user(
        &self,
        user_id: &UserId,
        force_filehash: bool,
        after: Option<u64>,
    ) -> Vec<Error> {
        let hashes = self
            .db
            .purge_and_get_hashes_from_user(user_id, force_filehash, after);

        purge_files(hashes)
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
    pub fn purge_from_server(
        &self,
        server_name: &ServerName,
        force_filehash: bool,
        after: Option<u64>,
    ) -> Vec<Error> {
        let hashes = self
            .db
            .purge_and_get_hashes_from_server(server_name, force_filehash, after);

        purge_files(hashes)
    }
}

/// Creates the media file, using the configured media backend
///
/// Note: this function does NOT set the metadata related to the file
pub async fn create_file(sha256_hex: &str, file: &[u8]) -> Result<()> {
    match &services().globals.config.media {
        MediaConfig::FileSystem {
            path,
            directory_structure,
        } => {
            let path = services()
                .globals
                .get_media_path(path, directory_structure, sha256_hex)?;

            let mut f = File::create(path).await?;
            f.write_all(file).await?;
        }
    }

    Ok(())
}

/// Fetches the file from the configured media backend
async fn get_file(sha256_hex: &str) -> Result<Vec<u8>> {
    Ok(match &services().globals.config.media {
        MediaConfig::FileSystem {
            path,
            directory_structure,
        } => {
            let path = services()
                .globals
                .get_media_path(path, directory_structure, sha256_hex)?;

            let mut file = Vec::new();
            File::open(path).await?.read_to_end(&mut file).await?;

            file
        }
    })
}

/// Purges the given files from the media backend
/// Returns a `Vec` of errors that occurred when attempting to delete the files
///
/// Note: this does NOT remove the related metadata from the database
fn purge_files(hashes: Vec<Result<String>>) -> Vec<Error> {
    hashes
        .into_iter()
        .map(|hash| match hash {
            Ok(v) => delete_file(&v),
            Err(e) => Err(e),
        })
        .filter_map(|r| if let Err(e) = r { Some(e) } else { None })
        .collect()
}

/// Deletes the given file from the media backend
///
/// Note: this does NOT remove the related metadata from the database
fn delete_file(sha256_hex: &str) -> Result<()> {
    match &services().globals.config.media {
        MediaConfig::FileSystem {
            path,
            directory_structure,
        } => {
            let mut path =
                services()
                    .globals
                    .get_media_path(path, directory_structure, sha256_hex)?;

            if let Err(e) = fs::remove_file(&path) {
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

                    if let Err(e) = fs::remove_dir(&path) {
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
        }
    }

    Ok(())
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
