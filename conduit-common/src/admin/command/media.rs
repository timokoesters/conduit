use std::{
    num::NonZeroUsize,
    time::{Duration, SystemTime},
};

use clap::{Args, Subcommand};
use ruma::{OwnedMxcUri, OwnedServerName, OwnedUserId};
use serde::{Deserialize, Serialize};

use crate::media::ServerNameOrUserId;

/// Commands to manage media, stored on the server or otherwise
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum Command {
    List(ListArgs),

    Purge(PurgeArgs),

    Block(BlockArgs),

    /// Shows information about the requested media
    Query {
        /// The MXC URI of the media you want to request information about
        mxc: OwnedMxcUri,
    },

    /// Displays the specified media file
    Show {
        /// The MXC URI of the media you want to view
        mxc: OwnedMxcUri,
    },

    /// Allows previously blocked media to be accessed again. Will also unblock media with the
    /// same SHA256 hash
    Unblock {
        mxc_uris: Vec<OwnedMxcUri>,
    },
}

/// Lists all the media matching the specified requirements
#[derive(Args, Debug, Serialize, Deserialize)]
#[command(args_conflicts_with_subcommands = true)]
pub struct ListArgs {
    #[command(subcommand)]
    pub subcommand: Option<ListCommand>,

    #[command(flatten)]
    pub user_server_filter: ListUserOrServerArgs,

    /// Whether to the list of thumbnails instead of full-sized media.
    ///
    /// This may be useful if you're looking for remote media to purge and/or block, as in the case
    /// of remote media, only the thumbnails may have been downloaded to the media store.
    #[arg(short = 't', long)]
    pub thumbnails: bool,

    /// The content-type media must have to be listed.
    /// if only a "type" (as opposed to "type/subtype") is specified,
    /// all media with that type are returned, no matter the sub-type.
    ///
    /// For example, if you request content-type "image", than files
    /// of content type "image/png", "image/jpeg", etc. will be returned.
    #[arg(short, long)]
    pub content_type: Option<String>,

    /// The point in time after which media had to be uploaded to be
    /// shown (in the UTC timezone).
    /// Should be in the format {YYYY}-{MM}-{DD}T{hh}:{mm}:{ss}
    #[arg(
        short = 'b', long,
        value_parser = humantime::parse_rfc3339_weak
    )]
    pub uploaded_before: Option<SystemTime>,

    /// The point in time before which media had to be uploaded to be
    /// shown (in the UTC timezone).
    /// Should be in the format {YYYY}-{MM}-{DD}T{hh}:{mm}:{ss}
    #[arg(
        short = 'a', long,
        value_parser = humantime::parse_rfc3339_weak
    )]
    pub uploaded_after: Option<SystemTime>,

    #[arg(default_value = "1")]
    /// The page of results to show
    pub page: NonZeroUsize,
}

#[derive(Args, Debug, Serialize, Deserialize)]
#[group(required = false)]
pub struct ListUserOrServerArgs {
    /// The user that uploaded the media.
    /// Only local media uploaders can be recorded, so specifying a non-local
    /// user will always yield no results
    #[arg(short, long)]
    pub user: Option<OwnedUserId>,

    /// The server from which the media originated from.
    /// If you want to list local media, just set this to
    /// be your own server's servername
    #[arg(short, long)]
    pub server: Option<OwnedServerName>,
}

impl From<ListUserOrServerArgs> for Option<ServerNameOrUserId> {
    fn from(value: ListUserOrServerArgs) -> Self {
        value
            .user
            .map(ServerNameOrUserId::UserId)
            .or_else(|| value.server.map(ServerNameOrUserId::ServerName))
    }
}

#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum ListCommand {
    /// Lists all media that is currently blocked
    Blocked {
        /// The page of results to show
        #[arg(default_value = "1")]
        page: NonZeroUsize,
    },
}

/// Purge a list of media, deleting them from the server's media storage, formatted as MXC URIs
///
/// Note: This will also delete media with the same sha256 hash, so
/// only use this when you are certain all the media is undesirable
#[derive(Args, Debug, Serialize, Deserialize)]
#[command(args_conflicts_with_subcommands = true)]
pub struct PurgeArgs {
    #[command(subcommand)]
    pub subcommand: Option<PurgeCommand>,

    /// The MXC URIs of the media to purge
    pub mxc_uris: Vec<OwnedMxcUri>,
}

#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum PurgeCommand {
    /// Purges all media uploaded by the local users specified.
    ///
    /// Note: This will also delete identical media uploaded by other users, so
    /// only use this when all the media they uploaded is undesirable
    FromUsers {
        /// The users to purge media from
        user_ids: Vec<OwnedUserId>,

        /// Only purge media uploaded in the last {timeframe}
        ///
        /// Should be in the form specified by humantime::parse_duration
        /// (e.g. 48h, 60min, 10days etc.)
        // --help is unformatted
        #[allow(rustdoc::bare_urls)]
        /// https://docs.rs/humantime/2.2.0/humantime/fn.parse_duration.html
        #[arg(
            long, short = 't',
            value_parser = humantime::parse_duration
        )]
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
    FromServer {
        /// The server from which media should be purged.
        server_name: OwnedServerName,

        /// Only purge media uploaded in the last {timeframe}
        ///
        /// Should be in the form specified by humantime::parse_duration
        /// (e.g. 48h, 60min, 10days etc.)
        // --help is unformatted
        #[allow(rustdoc::bare_urls)]
        /// https://docs.rs/humantime/2.2.0/humantime/fn.parse_duration.html
        #[arg(
            long, short = 't',
            value_parser = humantime::parse_duration
        )]
        from_last: Option<Duration>,

        #[arg(long, short)]
        /// Also deletes other media with the same SHA256 hash, ensuring that the file is removed from
        /// the media backend, so only use this when all the media they uploaded is undesirable
        force_filehash: bool,
    },
}

/// Prevents the list of media from being accessed, but does not delete the media if it
/// is already downloaded. If the media has already been downloaded, the sha256 hash
/// is blocked, meaning that any other current or future uploads/downloads of the exact same
/// content cannot be accessed either.
#[derive(Args, Debug, Serialize, Deserialize)]
#[command(args_conflicts_with_subcommands = true)]
pub struct BlockArgs {
    #[command(subcommand)]
    pub subcommand: Option<BlockCommand>,

    /// The MXC URIs of the media to block
    pub mxc_uris: Vec<OwnedMxcUri>,

    #[arg(long, short)]
    /// Prevents the specified media from being downloaded in the future
    ///
    /// Note: This will also delete identical media uploaded by other users, so
    /// only use this all the media is known to be undesirable
    pub and_purge: bool,
    #[arg(long, short)]
    /// Optional reason as to why this media should be blocked
    pub reason: Option<String>,
}

#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum BlockCommand {
    /// Prevents all media uploaded by the specified local users from being accessed
    ///
    /// Note: This will also block media with the same SHA256 hash, so
    /// only use this when all media uploaded by the user is undesirable (or if
    /// you only plan for the bloackage to be temporary)
    FromUsers {
        /// The users of which media uploaded by should be blocked
        user_ids: Vec<OwnedUserId>,

        /// Only block media uploaded in the last {timeframe}
        ///
        /// Should be in the form specified by humantime::parse_duration
        /// (e.g. 48h, 60min, 10days etc.)
        // --help is unformatted
        #[allow(rustdoc::bare_urls)]
        /// https://docs.rs/humantime/2.2.0/humantime/fn.parse_duration.html
        #[arg(
            long, short,
            value_parser = humantime::parse_duration
        )]
        from_last: Option<Duration>,

        /// Optional reason as to why this media should be blocked
        #[arg(long, short)]
        reason: Option<String>,
    },
}

#[derive(Serialize, Deserialize)]
pub struct File {
    pub file: Vec<u8>,
    pub content_type: Option<String>,
    pub filename: Option<String>,
    pub mxc: OwnedMxcUri,
}
