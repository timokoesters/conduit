use image::{imageops::FilterType, GenericImageView};

use crate::{utils, Error, Result};
use std::mem;

pub struct FileMeta {
    pub filename: Option<String>,
    pub content_type: Option<String>,
    pub file: Vec<u8>,
}

#[derive(Clone)]
pub struct Media {
    pub(super) mediaid_file: sled::Tree, // MediaId = MXC + WidthHeight + Filename + ContentType
}

impl Media {
    /// Uploads or replaces a file.
    pub fn create(
        &self,
        mxc: String,
        filename: &Option<&str>,
        content_type: &Option<&str>,
        file: &[u8],
    ) -> Result<()> {
        let mut key = mxc.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&0_u32.to_be_bytes()); // Width = 0 if it's not a thumbnail
        key.extend_from_slice(&0_u32.to_be_bytes()); // Height = 0 if it's not a thumbnail
        key.push(0xff);
        key.extend_from_slice(filename.as_ref().map(|f| f.as_bytes()).unwrap_or_default());
        key.push(0xff);
        key.extend_from_slice(
            content_type
                .as_ref()
                .map(|c| c.as_bytes())
                .unwrap_or_default(),
        );

        self.mediaid_file.insert(key, file)?;

        Ok(())
    }

    /// Uploads or replaces a file thumbnail.
    pub fn upload_thumbnail(
        &self,
        mxc: String,
        filename: &Option<String>,
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
        key.extend_from_slice(filename.as_ref().map(|f| f.as_bytes()).unwrap_or_default());
        key.push(0xff);
        key.extend_from_slice(
            content_type
                .as_ref()
                .map(|c| c.as_bytes())
                .unwrap_or_default(),
        );

        self.mediaid_file.insert(key, file)?;

        Ok(())
    }

    /// Downloads a file.
    pub fn get(&self, mxc: &str) -> Result<Option<FileMeta>> {
        let mut prefix = mxc.as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&0_u32.to_be_bytes()); // Width = 0 if it's not a thumbnail
        prefix.extend_from_slice(&0_u32.to_be_bytes()); // Height = 0 if it's not a thumbnail
        prefix.push(0xff);

        if let Some(r) = self.mediaid_file.scan_prefix(&prefix).next() {
            let (key, file) = r?;
            let mut parts = key.rsplit(|&b| b == 0xff);

            let content_type = parts
                .next()
                .map(|bytes| {
                    Ok::<_, Error>(utils::string_from_bytes(bytes).map_err(|_| {
                        Error::bad_database("Content type in mediaid_file is invalid unicode.")
                    })?)
                })
                .transpose()?;

            let filename_bytes = parts
                .next()
                .ok_or_else(|| Error::bad_database("Media ID in db is invalid."))?;

            let filename = if filename_bytes.is_empty() {
                None
            } else {
                Some(utils::string_from_bytes(filename_bytes).map_err(|_| {
                    Error::bad_database("Filename in mediaid_file is invalid unicode.")
                })?)
            };

            Ok(Some(FileMeta {
                filename,
                content_type,
                file: file.to_vec(),
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
    pub fn get_thumbnail(&self, mxc: String, width: u32, height: u32) -> Result<Option<FileMeta>> {
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

        if let Some(r) = self.mediaid_file.scan_prefix(&thumbnail_prefix).next() {
            // Using saved thumbnail
            let (key, file) = r?;
            let mut parts = key.rsplit(|&b| b == 0xff);

            let content_type = parts
                .next()
                .map(|bytes| {
                    Ok::<_, Error>(utils::string_from_bytes(bytes).map_err(|_| {
                        Error::bad_database("Content type in mediaid_file is invalid unicode.")
                    })?)
                })
                .transpose()?;

            let filename_bytes = parts
                .next()
                .ok_or_else(|| Error::bad_database("Media ID in db is invalid."))?;

            let filename = if filename_bytes.is_empty() {
                None
            } else {
                Some(
                    utils::string_from_bytes(filename_bytes)
                        .map_err(|_| Error::bad_database("Filename in db is invalid."))?,
                )
            };

            Ok(Some(FileMeta {
                filename,
                content_type,
                file: file.to_vec(),
            }))
        } else if let Some(r) = self.mediaid_file.scan_prefix(&original_prefix).next() {
            // Generate a thumbnail

            let (key, file) = r?;
            let mut parts = key.rsplit(|&b| b == 0xff);

            let content_type = parts
                .next()
                .map(|bytes| {
                    Ok::<_, Error>(utils::string_from_bytes(bytes).map_err(|_| {
                        Error::bad_database("Content type in mediaid_file is invalid unicode.")
                    })?)
                })
                .transpose()?;

            let filename_bytes = parts
                .next()
                .ok_or_else(|| Error::bad_database("Media ID in db is invalid."))?;

            let filename = if filename_bytes.is_empty() {
                None
            } else {
                Some(utils::string_from_bytes(filename_bytes).map_err(|_| {
                    Error::bad_database("Filename in mediaid_file is invalid unicode.")
                })?)
            };

            if let Ok(image) = image::load_from_memory(&file) {
                let original_width = image.width();
                let original_height = image.height();
                if width > original_width || height > original_height {
                    return Ok(Some(FileMeta {
                        filename,
                        content_type,
                        file: file.to_vec(),
                    }));
                }

                let thumbnail = if crop {
                    image.resize_to_fill(width, height, FilterType::Triangle)
                } else {
                    let (exact_width, exact_height) = {
                        // Copied from image::dynimage::resize_dimensions
                        let ratio = u64::from(original_width) * u64::from(height);
                        let nratio = u64::from(width) * u64::from(original_height);

                        let use_width = nratio > ratio;
                        let intermediate = if use_width {
                            u64::from(original_height) * u64::from(width) / u64::from(width)
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

                self.mediaid_file.insert(thumbnail_key, &*thumbnail_bytes)?;

                Ok(Some(FileMeta {
                    filename,
                    content_type,
                    file: thumbnail_bytes.to_vec(),
                }))
            } else {
                // Couldn't parse file to generate thumbnail, send original
                Ok(Some(FileMeta {
                    filename,
                    content_type,
                    file: file.to_vec(),
                }))
            }
        } else {
            Ok(None)
        }
    }
}
