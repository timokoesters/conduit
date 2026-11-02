use std::{
    num::NonZeroUsize,
    time::{Duration, SystemTime},
};

use bytesize::ByteSize;
use chrono::DateTime;
use clap::{Args, Subcommand};
use ruma::{
    OwnedEventId, OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId, OwnedServerName, OwnedUserId,
    RoomVersionId,
};
use serde::{Deserialize, Serialize};

use crate::media::{self, BlockedMediaInfo};

#[derive(Serialize, Deserialize)]
pub enum CommandOutput {
    Plain(String),
    TaskComplete(String),
    File(MediaFile),
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
        let dimensions = dimensions
            .map(|(w, h)| RowItem::Code(format!("{w}x{h}")))
            .unwrap_or_default();
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

        Self(vec![
            mxc_uri,
            dimensions,
            creation,
            user_id,
            content_type,
            filename,
            size,
        ])
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

#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum Command {
    /// List all rooms the server knows about
    ListRooms,

    /// List users in the database
    ListLocalUsers,

    /// List all rooms we are currently handling an incoming pdu from
    IncomingFederation,

    /// Removes an alias from the server
    RemoveAlias {
        /// The alias to be removed
        alias: OwnedRoomAliasId,
    },

    /// Deactivate a user
    ///
    /// User will not be removed from all rooms by default.
    /// Use --leave-rooms to force the user to leave all rooms.
    DeactivateUser {
        #[arg(short, long)]
        leave_rooms: bool,
        user_id: OwnedUserId,
        #[command(flatten)]
        purge_media: DeactivatePurgeMediaArgs,
    },

    /// Shows information about the requested media
    QueryMedia {
        /// The MXC URI of the media you want to request information about
        mxc: OwnedMxcUri,
    },

    /// Sends a message with the requested media attached, so that you can view it easily
    ShowMedia {
        /// The MXC URI of the media you want to view
        mxc: OwnedMxcUri,
    },

    /// Lists all the media matching the specified requirements
    ListMedia {
        #[command(flatten)]
        user_server_filter: ListMediaArgs,

        /// Whether to include thumbnails in the list.
        /// It is recommended to do so if you are not only looking
        /// for local media, as with remote media, the full media file
        /// might not be downloaded, just the thumbnail
        #[arg(short = 't', long)]
        include_thumbnails: bool,

        #[arg(short, long)]
        /// The content-type media must have to be listed.
        /// if only a "type" (as opposed to "type/subtype") is specified,
        /// all media with that type are returned, no matter the sub-type.
        ///
        /// For example, if you request content-type "image", than files
        /// of content type "image/png", "image/jpeg", etc. will be returned.
        content_type: Option<String>,

        #[arg(
            short = 'b', long,
            value_parser = humantime::parse_rfc3339_weak
        )]
        /// The point in time after which media had to be uploaded to be
        /// shown (in the UTC timezone).
        /// Should be in the format {YYYY}-{MM}-{DD}T{hh}:{mm}:{ss}
        uploaded_before: Option<SystemTime>,

        #[arg(
            short = 'a', long,
            value_parser = humantime::parse_rfc3339_weak
        )]
        /// The point in time before which media had to be uploaded to be
        /// shown (in the UTC timezone).
        /// Should be in the format {YYYY}-{MM}-{DD}T{hh}:{mm}:{ss}
        uploaded_after: Option<SystemTime>,

        #[arg(default_value = "1")]
        /// The page of results to show
        page: NonZeroUsize,
    },

    /// Purge a list of media, formatted as MXC URIs
    ///
    /// Note: This will also delete media with the same sha256 hash, so
    /// only use this when you are certain all the media is undesirable
    PurgeMedia { mxc_uris: Vec<OwnedMxcUri> },

    /// Purges all media uploaded by the local users specified.
    ///
    /// Note: This will also delete identical media uploaded by other users, so
    /// only use this when all the media they uploaded is undesirable
    PurgeMediaFromUsers {
        /// The users to purge media from
        user_ids: Vec<OwnedUserId>,

        #[arg(
            long, short = 't',
            value_parser = humantime::parse_duration
        )]
        /// Only purge media uploaded in the last {timeframe}
        ///
        /// Should be in the form specified by humantime::parse_duration
        /// (e.g. 48h, 60min, 10days etc.)
        // --help is unformatted
        #[allow(rustdoc::bare_urls)]
        /// https://docs.rs/humantime/2.2.0/humantime/fn.parse_duration.html
        from_last: Option<Duration>,

        #[arg(long, short)]
        /// Also deletes other media with the same SHA256 hash, ensuring that the file is removed from
        /// the media backend, so only use this when all the media they uploaded is undesirable
        force_filehash: bool,
    },

    /// Purges all media from the specified server
    ///
    /// Note: This will also delete identical media uploaded by local users, so
    /// only use this when all the media from that server is undesirable (or if
    /// you know that no media on the remote server is also uploaded locally)
    PurgeMediaFromServer {
        server_id: OwnedServerName,
        #[arg(
            long, short = 't',
            value_parser = humantime::parse_duration
        )]
        /// Only purge media uploaded in the last {timeframe}
        ///
        /// Should be in the form specified by humantime::parse_duration
        /// (e.g. 48h, 60min, 10days etc.)
        // --help is unformatted
        #[allow(rustdoc::bare_urls)]
        /// https://docs.rs/humantime/2.2.0/humantime/fn.parse_duration.html
        from_last: Option<Duration>,

        #[arg(long, short)]
        /// Also deletes other media with the same SHA256 hash, ensuring that the file is removed from
        /// the media backend, so only use this when all the media they uploaded is undesirable
        force_filehash: bool,
    },

    /// Prevents the list of media from being accessed, but does not delete the media if it
    /// is already downloaded. If the media has already been downloaded, the sha256 hash
    /// is blocked, meaning that any other current or future uploads/downloads of the exact same
    /// content cannot be accessed either.
    BlockMedia {
        mxc_uris: Vec<OwnedMxcUri>,

        #[arg(long, short)]
        /// Prevents the specified media from being downloaded in the future
        ///
        /// Note: This will also delete identical media uploaded by other users, so
        /// only use this all the media is known to be undesirable
        and_purge: bool,
        #[arg(long, short)]
        /// Optional reason as to why this media should be blocked
        reason: Option<String>,
    },

    /// Prevents all media uploaded by the specified local users from being accessed
    ///
    /// Note: This will also block media with the same SHA256 hash, so
    /// only use this when all media uploaded by the user is undesirable (or if
    /// you only plan for the bloackage to be temporary)
    BlockMediaFromUsers {
        user_ids: Vec<OwnedUserId>,

        #[arg(
            long, short,
            value_parser = humantime::parse_duration
        )]
        /// Only block media uploaded in the last {timeframe}
        ///
        /// Should be in the form specified by humantime::parse_duration
        /// (e.g. 48h, 60min, 10days etc.)
        // --help is unformatted
        #[allow(rustdoc::bare_urls)]
        /// https://docs.rs/humantime/2.2.0/humantime/fn.parse_duration.html
        from_last: Option<Duration>,
        #[arg(long, short)]
        /// Optional reason as to why this media should be blocked
        reason: Option<String>,
    },

    /// Lists all media that is currently blocked
    ListBlockedMedia {
        #[arg(default_value = "1")]
        /// The page of results to show
        page: NonZeroUsize,
    },

    /// Allows previously blocked media to be accessed again. Will also unblock media with the
    /// same SHA256 hash
    UnblockMedia { mxc_uris: Vec<OwnedMxcUri> },

    /// Get the auth_chain of a PDU
    GetAuthChain {
        /// An event ID (the $ character followed by the base64 reference hash)
        event_id: OwnedEventId,
    },

    /// Parse and print a PDU from a JSON, either from a codeblock if on matrix, or stdin if using
    /// the CLI
    ///
    /// The PDU event is only checked for validity and is not added to the
    /// database.
    ParsePdu,

    /// Retrieve and print a PDU by ID from the Conduit database
    GetPdu {
        /// An event ID (a $ followed by the base64 reference hash)
        event_id: OwnedEventId,
    },

    /// Print database memory usage statistics
    MemoryUsage,

    /// Clears all of Conduit's database caches with index smaller than the amount
    ClearDatabaseCaches { amount: u32 },

    /// Clears all of Conduit's service caches with index smaller than the amount
    ClearServiceCaches { amount: u32 },

    /// Reset user password
    ResetPassword {
        /// Username of the user for whom the password should be reset
        username: String,
    },

    /// Create a new user
    CreateUser {
        /// Username of the new user
        username: String,
        /// Password of the new user, if unspecified one is generated
        password: Option<String>,
    },

    /// Disables incoming federation handling for a room.
    DisableRoom { room_id: OwnedRoomId },
    /// Enables incoming federation handling for a room again.
    EnableRoom { room_id: OwnedRoomId },

    /// Sign a json object using Conduit's signing keys, putting the json in a codeblock if on
    /// matrix, or stdin if using the CLI
    SignJson,

    /// Verify json signatures, putting the json in a codeblock if on matrix, or stdin if using the
    /// CLI
    VerifyJson,

    /// Parses a JSON object as an event then creates a hash and signs it, putting a room
    /// version as an argument, and the json in a codeblock if on matrix, or stdin if using the CLI
    HashAndSignEvent { room_version_id: RoomVersionId },
}

impl Command {
    pub fn takes_text_input(&self) -> bool {
        matches!(
            self,
            Self::ParsePdu | Self::SignJson | Self::VerifyJson | Self::HashAndSignEvent { .. }
        )
    }
}

#[derive(Args, Debug, Serialize, Deserialize)]
#[group(multiple = true, required = false)]
pub struct DeactivatePurgeMediaArgs {
    #[arg(long, short = 'm')]
    /// Purges all media uploaded by the user(s) after deactivating their account
    pub purge_media: bool,

    #[arg(
        long, short = 't',
        value_parser = humantime::parse_duration,
        requires = "purge_media"
    )]
    /// If the --purge-media is present, it only purges media uploaded in the last {time-period}
    ///
    /// Should be in the form specified by humantime::parse_duration
    /// (e.g. 48h, 60min, 10days etc.)
    // --help is unformatted
    #[allow(rustdoc::bare_urls)]
    /// https://docs.rs/humantime/2.2.0/humantime/fn.parse_duration.html
    ///
    /// Note: This will also delete identical media uploaded by other users, so
    /// only use this when all the media they uploaded in this timeframe is undesirable
    pub media_from_last: Option<Duration>,

    #[arg(long, short = 'f', requires = "purge_media")]
    /// If the --purge-media is present, it will also delete identical media uploaded by other
    /// users, ensuring that the file is removed from the media backend, so only use this when all
    /// the media they uploaded is undesirable
    pub force_filehash: bool,
}

#[derive(Args, Debug, Serialize, Deserialize)]
#[group(required = false)]
pub struct ListMediaArgs {
    #[arg(short, long)]
    /// The user that uploaded the media.
    /// Only local media uploaders can be recorded, so specifying a non-local
    /// user will always yield no results
    pub user: Option<OwnedUserId>,

    #[arg(short, long)]
    /// The server from which the media originated from.
    /// If you want to list local media, just set this to
    /// be your own server's servername
    pub server: Option<OwnedServerName>,
}

#[derive(Serialize, Deserialize)]
pub struct MediaFile {
    pub file: Vec<u8>,
    pub content_type: Option<String>,
    pub filename: Option<String>,
    pub mxc: OwnedMxcUri,
}
