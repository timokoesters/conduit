use ruma::http_headers::ContentDisposition;

use crate::Result;

pub trait Data: Send + Sync {
    fn create_file_metadata(
        &self,
        mxc: String,
        width: u32,
        height: u32,
        content_disposition: &ContentDisposition,
        content_type: Option<&str>,
    ) -> Result<Vec<u8>>;

    /// Returns content_disposition, content_type and the metadata key.
    fn search_file_metadata(
        &self,
        mxc: String,
        width: u32,
        height: u32,
    ) -> Result<(ContentDisposition, Option<String>, Vec<u8>)>;
}
