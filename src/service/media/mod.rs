mod data;
use std::io::Cursor;

pub use data::Data;

use crate::{services, Result};
use image::imageops::FilterType;

use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
};

pub struct FileMeta {
    pub content_disposition: Option<String>,
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
        mxc: String,
        content_disposition: Option<&str>,
        content_type: Option<&str>,
        file: &[u8],
    ) -> Result<()> {
        // Width, Height = 0 if it's not a thumbnail
        let key = self
            .db
            .create_file_metadata(mxc, 0, 0, content_disposition, content_type)?;

        let path = services().globals.get_media_file(&key);
        let mut f = File::create(path).await?;
        f.write_all(file).await?;
        Ok(())
    }

    /// Uploads or replaces a file thumbnail.
    #[allow(clippy::too_many_arguments)]
    pub async fn upload_thumbnail(
        &self,
        mxc: String,
        content_disposition: Option<&str>,
        content_type: Option<&str>,
        width: u32,
        height: u32,
        file: &[u8],
    ) -> Result<()> {
        let key =
            self.db
                .create_file_metadata(mxc, width, height, content_disposition, content_type)?;

        let path = services().globals.get_media_file(&key);
        let mut f = File::create(path).await?;
        f.write_all(file).await?;

        Ok(())
    }

    /// Downloads a file.
    pub async fn get(&self, mxc: String) -> Result<Option<FileMeta>> {
        if let Ok((content_disposition, content_type, key)) =
            self.db.search_file_metadata(mxc, 0, 0)
        {
            let path = services().globals.get_media_file(&key);
            let mut file = Vec::new();
            BufReader::new(File::open(path).await?)
                .read_to_end(&mut file)
                .await?;

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
        width: u32,
        height: u32,
    ) -> Result<Option<FileMeta>> {
        let (width, height, crop) = self
            .thumbnail_properties(width, height)
            .unwrap_or((0, 0, false)); // 0, 0 because that's the original file

        if let Ok((content_disposition, content_type, key)) =
            self.db.search_file_metadata(mxc.clone(), width, height)
        {
            // Using saved thumbnail
            let path = services().globals.get_media_file(&key);
            let mut file = Vec::new();
            File::open(path).await?.read_to_end(&mut file).await?;

            Ok(Some(FileMeta {
                content_disposition,
                content_type,
                file: file.to_vec(),
            }))
        } else if let Ok((content_disposition, content_type, key)) =
            self.db.search_file_metadata(mxc.clone(), 0, 0)
        {
            // Generate a thumbnail
            let path = services().globals.get_media_file(&key);
            let mut file = Vec::new();
            File::open(path).await?.read_to_end(&mut file).await?;

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
                thumbnail.write_to(
                    &mut Cursor::new(&mut thumbnail_bytes),
                    image::ImageFormat::Png,
                )?;

                // Save thumbnail in database so we don't have to generate it again next time
                let thumbnail_key = self.db.create_file_metadata(
                    mxc,
                    width,
                    height,
                    content_disposition.as_deref(),
                    content_type.as_deref(),
                )?;

                let path = services().globals.get_media_file(&thumbnail_key);
                let mut f = File::create(path).await?;
                f.write_all(&thumbnail_bytes).await?;

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
