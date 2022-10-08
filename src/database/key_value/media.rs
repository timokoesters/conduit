use ruma::api::client::error::ErrorKind;

use crate::{database::KeyValueDatabase, service, utils, Error, Result};

impl service::media::Data for KeyValueDatabase {
    fn create_file_metadata(
        &self,
        mxc: String,
        width: u32,
        height: u32,
        content_disposition: Option<&str>,
        content_type: Option<&str>,
    ) -> Result<Vec<u8>> {
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

        self.mediaid_file.insert(&key, &[])?;

        Ok(key)
    }

    fn search_file_metadata(
        &self,
        mxc: String,
        width: u32,
        height: u32,
    ) -> Result<(Option<String>, Option<String>, Vec<u8>)> {
        let mut prefix = mxc.as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&width.to_be_bytes());
        prefix.extend_from_slice(&height.to_be_bytes());
        prefix.push(0xff);

        let (key, _) = self
            .mediaid_file
            .scan_prefix(prefix)
            .next()
            .ok_or(Error::BadRequest(ErrorKind::NotFound, "Media not found"))?;

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
                    Error::bad_database("Content Disposition in mediaid_file is invalid unicode.")
                })?,
            )
        };
        Ok((content_disposition, content_type, key))
    }
}
