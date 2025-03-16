mod data;
use std::io::Cursor;

pub use data::Data;
use ruma::{
    api::client::{error::ErrorKind, media::is_safe_inline_content_type},
    http_headers::{ContentDisposition, ContentDispositionType},
    ServerName,
};
use sha2::{digest::Output, Digest, Sha256};

use crate::{config::MediaConfig, services, Error, Result};
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
    ) -> Result<()> {
        let (sha256_digest, sha256_hex) = generate_digests(file);

        self.db.create_file_metadata(
            sha256_digest,
            size(file)?,
            servername,
            media_id,
            filename,
            content_type,
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
    pub async fn get(&self, servername: &ServerName, media_id: &str) -> Result<Option<FileMeta>> {
        let DbFileMeta {
            sha256_digest,
            filename,
            content_type,
            unauthenticated_access_permitted: _,
        } = self.db.search_file_metadata(servername, media_id)?;

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
    ) -> Result<Option<FileMeta>> {
        if let Some((width, height, crop)) = self.thumbnail_properties(width, height) {
            if let Ok(DbFileMeta {
                sha256_digest,
                filename,
                content_type,
                unauthenticated_access_permitted: _,
            }) = self
                .db
                .search_thumbnail_metadata(servername, media_id, width, height)
            {
                // Using saved thumbnail
                let file = get_file(&hex::encode(sha256_digest)).await?;

                Ok(Some(FileMeta {
                    content_disposition: content_disposition(filename, &content_type),
                    content_type,
                    file,
                }))
            } else if let Ok(DbFileMeta {
                sha256_digest,
                filename,
                content_type,
                unauthenticated_access_permitted: _,
            }) = self.db.search_file_metadata(servername, media_id)
            {
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
                unauthenticated_access_permitted: _,
            }) = self.db.search_file_metadata(servername, media_id)
            else {
                return Ok(None);
            };

            let file = get_file(&hex::encode(sha256_digest)).await?;

            Ok(Some(FileMeta {
                content_disposition: content_disposition(filename, &content_type),
                content_type,
                file,
            }))
        }
    }
}

/// Creates the media file, using the configured media backend
///
/// Note: this function does NOT set the metadata related to the file
pub async fn create_file(sha256_hex: &str, file: &[u8]) -> Result<()> {
    match &services().globals.config.media {
        MediaConfig::FileSystem { path } => {
            let path = services().globals.get_media_path(path, sha256_hex);

            let mut f = File::create(path).await?;
            f.write_all(file).await?;
        }
    }

    Ok(())
}

/// Fetches the file from the configured media backend
async fn get_file(sha256_hex: &str) -> Result<Vec<u8>> {
    Ok(match &services().globals.config.media {
        MediaConfig::FileSystem { path } => {
            let path = services().globals.get_media_path(path, sha256_hex);

            let mut file = Vec::new();
            File::open(path).await?.read_to_end(&mut file).await?;

            file
        }
    })
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
