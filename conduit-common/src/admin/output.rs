use std::num::NonZeroUsize;

use bytesize::ByteSize;
use chrono::DateTime;
use serde::{Deserialize, Serialize};

use crate::media::{self, BlockedMediaInfo};

use super::command::media::File;

#[derive(Serialize, Deserialize)]
pub enum CommandOutput {
    Plain(String),
    TaskComplete(String),
    File(File),
    Table {
        title: String,
        headers: Vec<String>,
        page: NonZeroUsize,
        rows: Vec<Row>,
    },
    CodeBlockWithMsg {
        code_block: CodeBlock,
        message: String,
    },
}

#[derive(Serialize, Deserialize)]
pub struct CodeBlock {
    pub code: String,
    pub language: String,
}

impl From<String> for CommandOutput {
    fn from(value: String) -> Self {
        Self::Plain(value)
    }
}

impl From<&str> for CommandOutput {
    fn from(value: &str) -> Self {
        value.to_owned().into()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Row(Vec<RowItem>);

impl Row {
    pub fn get(self) -> Vec<RowItem> {
        self.0
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub enum RowItem {
    Plain(String),
    Code(String),
    #[default]
    Empty,
}

impl RowItem {
    pub fn plain(value: impl ToString) -> Self {
        Self::Plain(value.to_string())
    }

    pub fn code(value: impl ToString) -> Self {
        Self::Code(value.to_string())
    }
}

impl From<media::ListItem> for Row {
    fn from(value: media::ListItem) -> Self {
        let media::ListItem {
            server_name,
            media_id,
            uploader_localpart,
            content_type,
            filename,
            dimensions,
            size,
            creation,
        } = value;

        let mxc_uri = RowItem::Code(format!("mxc://{server_name}/{media_id}"));
        let dimensions = dimensions.map(|(w, h)| RowItem::Code(format!("{w}x{h}")));
        let creation = RowItem::code(
            DateTime::from_timestamp(creation.try_into().unwrap_or(i64::MAX), 0)
                .expect("Timestamp is within range"),
        );
        let user_id = uploader_localpart
            .map(|localpart| RowItem::Code(format!("@{localpart}:{server_name}")))
            .unwrap_or_default();
        let content_type = content_type.map(RowItem::Code).unwrap_or_default();
        let filename = filename.map(RowItem::Code).unwrap_or_default();
        let size = RowItem::plain(ByteSize::b(size).display().si());

        let mut row = vec![mxc_uri, creation, user_id, content_type, filename, size];

        if let Some(dimensions) = dimensions {
            row.insert(1, dimensions);
        }

        Self(row)
    }
}

impl From<BlockedMediaInfo> for Row {
    fn from(value: BlockedMediaInfo) -> Self {
        let BlockedMediaInfo {
            server_name,
            media_id,
            unix_secs,
            reason,
            sha256_hex,
        } = value;

        let sha256_hash = sha256_hex.map(RowItem::Code).unwrap_or_default();
        let mxc_uri = RowItem::Code(format!("mxc://{server_name}/{media_id}"));
        let time = RowItem::code(
            i64::try_from(unix_secs)
                .map(|unix_secs| DateTime::from_timestamp(unix_secs, 0))
                .ok()
                .flatten()
                .expect("Time is valid"),
        );
        let reason = reason.map(RowItem::Code).unwrap_or_default();

        Self(vec![sha256_hash, mxc_uri, time, reason])
    }
}
