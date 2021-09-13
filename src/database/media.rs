use crate::database::globals::Globals;
use image::{imageops::FilterType, GenericImageView};

use super::abstraction::Tree;
use crate::{utils, Error, Result};
use std::{mem, sync::Arc};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt},
};

pub struct FileMeta {
    pub content_disposition: Option<String>,
    pub content_type: Option<String>,
    pub file: Vec<u8>,
}

pub struct Media {
    pub(super) mediaid_file: Arc<dyn Tree>, // MediaId = MXC + WidthHeight + ContentDisposition + ContentType
}

impl Media {
    /// Uploads a file.
    pub async fn create(
        &self,
        mxc: String,
        globals: &Globals,
        content_disposition: &Option<&str>,
        content_type: &Option<&str>,
        file: &[u8],
    ) -> Result<()> {
        let mut key = mxc.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&0_u32.to_be_bytes()); // Width = 0 if it's not a thumbnail
        key.extend_from_slice(&0_u32.to_be_bytes()); // Height = 0 if it's not a thumbnail
        key.push(0xff);
        key.extend_from_slice(
            content_disposition
                .as_ref()
                .map(|f| f.as_bytes())
                .unwrap_or_default(),
        );
        key.push(0xff);
        key.extend_from_slice(
            content_type
                .as_ref()
                .map(|c| c.as_bytes())
                .unwrap_or_default(),
        );

        let path = globals.get_media_file(&key);
        let mut f = File::create(path).await?;
        f.write_all(file).await?;

        self.mediaid_file.insert(&key, &[])?;
        Ok(())
    }

    /// Uploads or replaces a file thumbnail.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_thumbnail(
        &self,
        mxc: String,
        globals: &Globals,
        content_disposition: &Option<String>,
        content_type: &Option<String>,
        width: u32,
        height: u32,
        file: &[u8],
    ) -> Result<()> {
        let mut key = mxc.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&width.to_be_bytes());
        key.extend_from_slice(&height.to_be_bytes());
        key.push(0xff);
        key.extend_from_slice(
            content_disposition
                .as_ref()
                .map(|f| f.as_bytes())
                .unwrap_or_default(),
        );
        key.push(0xff);
        key.extend_from_slice(
            content_type
                .as_ref()
                .map(|c| c.as_bytes())
                .unwrap_or_default(),
        );

        let path = globals.get_media_file(&key);
        let mut f = File::create(path).await?;
        f.write_all(file).await?;

        self.mediaid_file.insert(&key, &[])?;

        Ok(())
    }

    /// Downloads a file.
    pub async fn get(&self, globals: &Globals, mxc: &str) -> Result<Option<FileMeta>> {
        let mut prefix = mxc.as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&0_u32.to_be_bytes()); // Width = 0 if it's not a thumbnail
        prefix.extend_from_slice(&0_u32.to_be_bytes()); // Height = 0 if it's not a thumbnail
        prefix.push(0xff);

        let first = self.mediaid_file.scan_prefix(prefix).next();
        if let Some((key, _)) = first {
            let path = globals.get_media_file(&key);
            let mut file = Vec::new();
            File::open(path).await?.read_to_end(&mut file).await?;
            let mut parts = key.rsplit(|&b| b == 0xff);

            let content_type = parts
                .next()
                .map(|bytes| {
                    utils::string_from_bytes(bytes).map_err(|_| {
                        Error::bad_database("Content type in mediaid_file is invalid unicode.")
                    })
                })
                .transpose()?;

            let content_disposition_bytes = parts
                .next()
                .ok_or_else(|| Error::bad_database("Media ID in db is invalid."))?;

            let content_disposition = if content_disposition_bytes.is_empty() {
                None
            } else {
                Some(
                    utils::string_from_bytes(content_disposition_bytes).map_err(|_| {
                        Error::bad_database(
                            "Content Disposition in mediaid_file is invalid unicode.",
                        )
                    })?,
                )
            };

            Ok(Some(FileMeta {
                content_disposition,
                content_type,
                file,
            }))
        } else {
            Ok(None)
        }
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
        mxc: String,
        globals: &Globals,
        width: u32,
        height: u32,
    ) -> Result<Option<FileMeta>> {
        let (width, height, crop) = self
            .thumbnail_properties(width, height)
            .unwrap_or((0, 0, false)); // 0, 0 because that's the original file

        let mut main_prefix = mxc.as_bytes().to_vec();
        main_prefix.push(0xff);

        let mut thumbnail_prefix = main_prefix.clone();
        thumbnail_prefix.extend_from_slice(&width.to_be_bytes());
        thumbnail_prefix.extend_from_slice(&height.to_be_bytes());
        thumbnail_prefix.push(0xff);

        let mut original_prefix = main_prefix;
        original_prefix.extend_from_slice(&0_u32.to_be_bytes()); // Width = 0 if it's not a thumbnail
        original_prefix.extend_from_slice(&0_u32.to_be_bytes()); // Height = 0 if it's not a thumbnail
        original_prefix.push(0xff);

        let first_thumbnailprefix = self.mediaid_file.scan_prefix(thumbnail_prefix).next();
        let first_originalprefix = self.mediaid_file.scan_prefix(original_prefix).next();
        if let Some((key, _)) = first_thumbnailprefix {
            // Using saved thumbnail
            let path = globals.get_media_file(&key);
            let mut file = Vec::new();
            File::open(path).await?.read_to_end(&mut file).await?;
            let mut parts = key.rsplit(|&b| b == 0xff);

            let content_type = parts
                .next()
                .map(|bytes| {
                    utils::string_from_bytes(bytes).map_err(|_| {
                        Error::bad_database("Content type in mediaid_file is invalid unicode.")
                    })
                })
                .transpose()?;

            let content_disposition_bytes = parts
                .next()
                .ok_or_else(|| Error::bad_database("Media ID in db is invalid."))?;

            let content_disposition = if content_disposition_bytes.is_empty() {
                None
            } else {
                Some(
                    utils::string_from_bytes(content_disposition_bytes).map_err(|_| {
                        Error::bad_database("Content Disposition in db is invalid.")
                    })?,
                )
            };

            Ok(Some(FileMeta {
                content_disposition,
                content_type,
                file: file.to_vec(),
            }))
        } else if let Some((key, _)) = first_originalprefix {
            // Generate a thumbnail
            let path = globals.get_media_file(&key);
            let mut file = Vec::new();
            File::open(path).await?.read_to_end(&mut file).await?;

            let mut parts = key.rsplit(|&b| b == 0xff);

            let content_type = parts
                .next()
                .map(|bytes| {
                    utils::string_from_bytes(bytes).map_err(|_| {
                        Error::bad_database("Content type in mediaid_file is invalid unicode.")
                    })
                })
                .transpose()?;

            let content_disposition_bytes = parts
                .next()
                .ok_or_else(|| Error::bad_database("Media ID in db is invalid."))?;

            let content_disposition = if content_disposition_bytes.is_empty() {
                None
            } else {
                Some(
                    utils::string_from_bytes(content_disposition_bytes).map_err(|_| {
                        Error::bad_database(
                            "Content Disposition in mediaid_file is invalid unicode.",
                        )
                    })?,
                )
            };

            if let Ok(image) = image::load_from_memory(&file) {
                let original_width = image.width();
                let original_height = image.height();
                if width > original_width || height > original_height {
                    return Ok(Some(FileMeta {
                        content_disposition,
                        content_type,
                        file: file.to_vec(),
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
                            if intermediate <= u64::from(::std::u32::MAX) {
                                (width, intermediate as u32)
                            } else {
                                (
                                    (u64::from(width) * u64::from(::std::u32::MAX) / intermediate)
                                        as u32,
                                    ::std::u32::MAX,
                                )
                            }
                        } else if intermediate <= u64::from(::std::u32::MAX) {
                            (intermediate as u32, height)
                        } else {
                            (
                                ::std::u32::MAX,
                                (u64::from(height) * u64::from(::std::u32::MAX) / intermediate)
                                    as u32,
                            )
                        }
                    };

                    image.thumbnail_exact(exact_width, exact_height)
                };

                let mut thumbnail_bytes = Vec::new();
                thumbnail.write_to(&mut thumbnail_bytes, image::ImageOutputFormat::Png)?;

                // Save thumbnail in database so we don't have to generate it again next time
                let mut thumbnail_key = key.to_vec();
                let width_index = thumbnail_key
                    .iter()
                    .position(|&b| b == 0xff)
                    .ok_or_else(|| Error::bad_database("Media in db is invalid."))?
                    + 1;
                let mut widthheight = width.to_be_bytes().to_vec();
                widthheight.extend_from_slice(&height.to_be_bytes());

                thumbnail_key.splice(
                    width_index..width_index + 2 * mem::size_of::<u32>(),
                    widthheight,
                );

                let path = globals.get_media_file(&thumbnail_key);
                let mut f = File::create(path).await?;
                f.write_all(&thumbnail_bytes).await?;

                self.mediaid_file.insert(&thumbnail_key, &[])?;

                Ok(Some(FileMeta {
                    content_disposition,
                    content_type,
                    file: thumbnail_bytes.to_vec(),
                }))
            } else {
                // Couldn't parse file to generate thumbnail, send original
                Ok(Some(FileMeta {
                    content_disposition,
                    content_type,
                    file: file.to_vec(),
                }))
            }
        } else {
            Ok(None)
        }
    }
}
