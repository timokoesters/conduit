use std::{
    borrow::Cow,
    collections::BTreeMap,
    num::NonZeroUsize,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytesize::ByteSize;
use chrono::DateTime;
use clap::{Args, Subcommand};
use image::GenericImageView;
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedMxcUri, OwnedRoomAliasId, OwnedRoomId,
    OwnedServerName, OwnedUserId, RoomId, RoomVersionId, UserId,
    events::room::{
        MediaSource,
        message::{FileMessageEventContent, ImageMessageEventContent},
    },
    profile::ProfileFieldValue,
    room_version_rules::RoomVersionRules,
};
use thiserror::Error;
use tokio::{sync::RwLock, time::Instant};

use crate::{
    Error as ConduitError, PduEvent, Result,
    api::client_server::{self, AUTO_GEN_PASSWORD_LENGTH, leave_all_rooms},
    service::globals::SigningKeys,
    services, utils,
};

use super::{
    media::{
        BlockedMediaInfo, FileInfo, MediaListItem, MediaQuery, MediaQueryFileInfo,
        MediaQueryThumbInfo, ServerNameOrUserId, size,
    },
    rate_limiting::Target,
};

pub mod room;

static PAGE_SIZE: usize = 50;

pub enum CommandOutput {
    Plain(String),
    TaskComplete(&'static str),
    Image(ImageMessageEventContent),
    File(FileMessageEventContent),
    Table {
        title: &'static str,
        headers: &'static [&'static str],
        page: NonZeroUsize,
        rows: Vec<Row>,
    },
    CodeBlockWithMsg {
        code_block: CodeBlock,
        message: String,
    },
}

pub struct CodeBlock {
    pub code: String,
    pub language: &'static str,
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

#[derive(Clone)]
pub struct Row(Vec<RowItem>);

impl Row {
    pub fn get(self) -> Vec<RowItem> {
        self.0
    }
}

#[derive(Clone, Default)]
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

impl From<MediaListItem> for Row {
    fn from(value: MediaListItem) -> Self {
        let MediaListItem {
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

#[derive(Error, Debug)]
pub enum Error {
    #[error("{0}")]
    Plain(String),

    #[error("No input block specified")]
    MissingInput,
}

impl Error {
    pub fn plain(value: impl ToString) -> ConduitError {
        ConduitError::AdminCommand(Error::Plain(value.to_string()))
    }
}

#[derive(Subcommand, Debug)]
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

    /// Verify json signatures, putting the json in a codeblock if on matrix, or stdin if using
    /// the CLI
    VerifyJson,

    /// Parses a JSON object as an event then creates a hash and signs it, putting a room
    /// version as an argument, and the json in a codeblock if on matrix, or stdin if using the CLI
    HashAndSignEvent { room_version_id: RoomVersionId },
}

#[derive(Args, Debug)]
#[group(multiple = true, required = false)]
pub struct DeactivatePurgeMediaArgs {
    #[arg(long, short = 'm')]
    /// Purges all media uploaded by the user(s) after deactivating their account
    purge_media: bool,

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
    media_from_last: Option<Duration>,

    #[arg(long, short = 'f', requires = "purge_media")]
    /// If the --purge-media is present, it will also delete identical media uploaded by other
    /// users, ensuring that the file is removed from the media backend, so only use this when all
    /// the media they uploaded is undesirable
    force_filehash: bool,
}

#[derive(Args, Debug)]
#[group(required = false)]
pub struct ListMediaArgs {
    #[arg(short, long)]
    /// The user that uploaded the media.
    /// Only local media uploaders can be recorded, so specifying a non-local
    /// user will always yield no results
    user: Option<OwnedUserId>,

    #[arg(short, long)]
    /// The server from which the media originated from.
    /// If you want to list local media, just set this to
    /// be your own server's servername
    server: Option<OwnedServerName>,
}

pub struct Service {
    pub room: Arc<room::Service>,
}

impl Service {
    pub fn build() -> Self {
        Self {
            room: room::Service::build(),
        }
    }

    pub async fn process_command(
        &self,
        command: Command,
        text_input: Option<String>,
    ) -> Result<CommandOutput> {
        Ok(match command {
            Command::ListRooms => {
                let room_ids = services().rooms.metadata.iter_ids();
                let output = format!(
                    "Rooms:\n{}",
                    room_ids
                        .filter_map(|r| r.ok())
                        .map(|id| id.to_string()
                            + "\tMembers: "
                            + &services()
                                .rooms
                                .state_cache
                                .room_joined_count(&id)
                                .ok()
                                .flatten()
                                .unwrap_or(0)
                                .to_string())
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                output.into()
            }
            Command::ListLocalUsers => match services().users.list_local_users() {
                Ok(users) => {
                    let mut msg: String = format!("Found {} local user account(s):\n", users.len());
                    msg += &users.join("\n");
                    msg
                }
                Err(e) => e.to_string(),
            }
            .into(),
            Command::IncomingFederation => {
                let map = services().globals.roomid_federationhandletime.read().await;
                let mut msg: String = format!("Handling {} incoming pdus:\n", map.len());

                for (r, (e, i)) in map.iter() {
                    let elapsed = i.elapsed();
                    msg += &format!(
                        "{} {}: {}m{}s\n",
                        r,
                        e,
                        elapsed.as_secs() / 60,
                        elapsed.as_secs() % 60
                    );
                }
                msg.into()
            }
            Command::GetAuthChain { event_id } => {
                if let Some(event) = services().rooms.timeline.get_pdu_json(&event_id)? {
                    let room_id_str = event
                        .get("room_id")
                        .and_then(|val| val.as_str())
                        .ok_or_else(|| ConduitError::bad_database("Invalid event in database"))?;

                    let room_id = <&RoomId>::try_from(room_id_str).map_err(|_| {
                        ConduitError::bad_database("Invalid room id field in event in database")
                    })?;
                    let start = Instant::now();
                    let count = services()
                        .rooms
                        .auth_chain
                        .get_auth_chain(room_id, vec![event_id])
                        .await?
                        .count();
                    let elapsed = start.elapsed();
                    CommandOutput::Plain(format!(
                        "Loaded auth chain with length {count} in {elapsed:?}"
                    ))
                } else {
                    return Err(Error::plain("Event not found"));
                }
            }
            Command::ParsePdu => {
                let Some(input) = text_input else {
                    return Err(Error::MissingInput.into());
                };
                match serde_json::from_str(&input) {
                    Ok(value) => {
                        match ruma::signatures::reference_hash(&value, &RoomVersionRules::V11) {
                            Ok(hash) => {
                                let event_id = EventId::parse(format!("${hash}"));

                                match serde_json::from_value::<PduEvent>(
                                    serde_json::to_value(value).expect("value is json"),
                                ) {
                                    Ok(pdu) => format!("EventId: {event_id:?}\n{pdu:#?}").into(),
                                    Err(e) => {
                                        return Err(Error::plain(format!(
                                            "EventId: {event_id:?}\nCould not parse event: {e}"
                                        )));
                                    }
                                }
                            }
                            Err(e) => {
                                return Err(Error::plain(format!(
                                    "Could not parse PDU JSON: {e:?}"
                                )));
                            }
                        }
                    }
                    Err(e) => {
                        return Err(Error::plain(format!("Invalid json in command body: {e}")));
                    }
                }
            }
            Command::GetPdu { event_id } => {
                let mut outlier = false;
                let mut pdu_json = services()
                    .rooms
                    .timeline
                    .get_non_outlier_pdu_json(&event_id)?;
                if pdu_json.is_none() {
                    outlier = true;
                    pdu_json = services().rooms.timeline.get_pdu_json(&event_id)?;
                }
                match pdu_json {
                    Some(json) => {
                        let json_text = serde_json::to_string_pretty(&json)
                            .expect("canonical json is valid json");
                        CommandOutput::CodeBlockWithMsg {
                            code_block: CodeBlock {
                                code: json_text,
                                language: "json",
                            },
                            message: String::from(if outlier {
                                "PDU is outlier"
                            } else {
                                "PDU was accepted"
                            }),
                        }
                    }
                    None => return Err(Error::plain("PDU not found.")),
                }
            }
            Command::MemoryUsage => {
                let response1 = services().memory_usage().await;
                let response2 = services().globals.db.memory_usage();

                CommandOutput::Plain(format!("Services:\n{response1}\n\nDatabase:\n{response2}"))
            }
            Command::ClearDatabaseCaches { amount } => {
                services().globals.db.clear_caches(amount);

                CommandOutput::TaskComplete("Cleared database caches")
            }
            Command::ClearServiceCaches { amount } => {
                services().clear_caches(amount).await;

                CommandOutput::TaskComplete("Cleared service caches")
            }
            Command::ResetPassword { username } => {
                let user_id = match UserId::parse_with_server_name(
                    username.as_str().to_lowercase(),
                    services().globals.server_name(),
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        return Err(Error::plain(format!(
                            "The supplied username is not a valid username: {e}"
                        )));
                    }
                };

                // Checks if user is local
                if user_id.server_name() != services().globals.server_name() {
                    return Err(Error::plain("The specified user is not from this server!"));
                };

                // Check if the specified user is valid
                if !services().users.exists(&user_id)?
                    || user_id
                        == UserId::parse_with_server_name(
                            "conduit",
                            services().globals.server_name(),
                        )
                        .expect("conduit user exists")
                {
                    return Err(Error::plain("The specified user does not exist!"));
                }

                let new_password = utils::random_string(AUTO_GEN_PASSWORD_LENGTH);

                match services()
                    .users
                    .set_password(&user_id, Some(new_password.as_str()))
                {
                    Ok(()) => CommandOutput::Plain(format!(
                        "Successfully reset the password for user {user_id}: {new_password}"
                    )),
                    Err(e) => {
                        return Err(Error::plain(format!(
                            "Couldn't reset the password for user {user_id}: {e}"
                        )));
                    }
                }
            }
            Command::CreateUser { username, password } => {
                let password =
                    password.unwrap_or_else(|| utils::random_string(AUTO_GEN_PASSWORD_LENGTH));
                // Validate user id
                let user_id = match UserId::parse_with_server_name(
                    username.as_str().to_lowercase(),
                    services().globals.server_name(),
                ) {
                    Ok(id) => id,
                    Err(e) => {
                        return Err(Error::plain(format!(
                            "The supplied username is not a valid username: {e}"
                        )));
                    }
                };

                // Checks if user is local
                if user_id.server_name() != services().globals.server_name() {
                    return Err(Error::plain("The specified user is not from this server!"));
                };

                if let Err(e) = user_id.validate_strict() {
                    return Err(Error::plain(format!("Userid {user_id} is not allowed {e}")));
                }
                if services().users.exists(&user_id)? {
                    return Err(Error::plain(format!("Userid {user_id} already exists")));
                }
                // Create user
                services().users.create(&user_id, Some(password.as_str()))?;

                // Default to pretty displayname
                let mut displayname = user_id.localpart().to_owned();

                // If enabled append lightning bolt to display name (default true)
                if services().globals.enable_lightning_bolt() {
                    displayname.push_str(" ⚡️");
                }

                services()
                    .users
                    .set_profile_field(&user_id, &ProfileFieldValue::DisplayName(displayname))?;

                // Initial account data
                services().account_data.update(
                    None,
                    &user_id,
                    ruma::events::GlobalAccountDataEventType::PushRules
                        .to_string()
                        .into(),
                    &serde_json::to_value(ruma::events::push_rules::PushRulesEvent {
                        content: ruma::events::push_rules::PushRulesEventContent {
                            global: ruma::push::Ruleset::server_default(&user_id),
                        },
                    })
                    .expect("to json value always works"),
                )?;

                // we dont add a device since we're not the user, just the creator

                // Inhibit login does not work for guests
                format!("Created user with user_id: {user_id} and password: {password}").into()
            }
            Command::DisableRoom { room_id } => {
                services().rooms.metadata.disable_room(&room_id, true)?;
                CommandOutput::TaskComplete("Room disabled")
            }
            Command::EnableRoom { room_id } => {
                services().rooms.metadata.disable_room(&room_id, false)?;
                CommandOutput::TaskComplete("Room enabled")
            }
            Command::DeactivateUser {
                leave_rooms,
                user_id,
                purge_media,
            } => {
                if !services().users.exists(&user_id)? {
                    return Err(Error::plain(format!(
                        "User {user_id} doesn't exist on this server"
                    )));
                } else if user_id.server_name() != services().globals.server_name() {
                    return Err(Error::plain(format!(
                        "User {user_id} is not from this server"
                    )));
                } else {
                    services().users.deactivate_account(&user_id)?;

                    if leave_rooms {
                        leave_all_rooms(&user_id).await?;
                    }

                    let failed_purged_media = if purge_media.purge_media {
                        let after = purge_media
                            .media_from_last
                            .map(unix_secs_from_duration)
                            .transpose()?;

                        services()
                            .media
                            .purge_from_user(&user_id, purge_media.force_filehash, after)
                            .await
                            .len()
                    } else {
                        0
                    };

                    if failed_purged_media == 0 {
                        format!("User {user_id} has been deactivated").into()
                    } else {
                        return Err(Error::plain(format!(
                            "User {user_id} has been deactivated, but {failed_purged_media} media failed to be purged, check the logs for more details"
                        )));
                    }
                }
            }
            Command::QueryMedia { mxc } => {
                let Ok((server_name, media_id)) = mxc.parts() else {
                    return Err(Error::plain("Invalid media MXC"));
                };

                let MediaQuery {
                    is_blocked,
                    source_file,
                    thumbnails,
                } = services().media.query(server_name, media_id)?;
                let mut message = format!("Is blocked Media ID: {is_blocked}");

                if let Some(MediaQueryFileInfo {
                    uploader_localpart,
                    sha256_hex,
                    filename,
                    content_type,
                    unauthenticated_access_permitted,
                    is_blocked_via_filehash,
                    file_info: time_info,
                }) = source_file
                {
                    message.push_str("\n\nInformation on full (non-thumbnail) file:\n");

                    if let Some(FileInfo {
                        creation,
                        last_access,
                        size,
                    }) = time_info
                    {
                        message.push_str(&format!("\nIs stored: true\nCreated at: {}\nLast accessed at: {}\nSize of file: {}",
                            DateTime::from_timestamp(creation.try_into().unwrap_or(i64::MAX),0).expect("Timestamp is within range"),
                            DateTime::from_timestamp(last_access.try_into().unwrap_or(i64::MAX),0).expect("Timestamp is within range"),
                            ByteSize::b(size).display().si()
                        ));
                    } else {
                        message.push_str("\nIs stored: false");
                    }

                    message.push_str(&format!("\nIs accessible via unauthenticated media endpoints: {unauthenticated_access_permitted}"));
                    message.push_str(&format!("\nSHA256 hash of file: {sha256_hex}"));
                    message.push_str(&format!("\nIs blocked due to sharing SHA256 hash with blocked media: {is_blocked_via_filehash}"));

                    if let Some(localpart) = uploader_localpart {
                        message.push_str(&format!("\nUploader: @{localpart}:{server_name}"))
                    }
                    if let Some(filename) = filename {
                        message.push_str(&format!("\nFilename: {filename}"))
                    }
                    if let Some(content_type) = content_type {
                        message.push_str(&format!("\nContent-type: {content_type}"))
                    }
                }

                if !thumbnails.is_empty() {
                    message.push_str("\n\nInformation on thumbnails of media:");
                }

                for MediaQueryThumbInfo {
                    width,
                    height,
                    sha256_hex,
                    filename,
                    content_type,
                    unauthenticated_access_permitted,
                    is_blocked_via_filehash,
                    file_info,
                } in thumbnails
                {
                    message.push_str(&format!("\n\nDimensions: {width}x{height}"));
                    if let Some(FileInfo {
                        creation,
                        last_access,
                        size,
                    }) = file_info
                    {
                        message.push_str(&format!("\nIs stored: true\nCreated at: {}\nLast accessed at: {}\nSize of file: {}",
                            DateTime::from_timestamp(creation.try_into().unwrap_or(i64::MAX),0).expect("Timestamp is within range"),
                            DateTime::from_timestamp(last_access.try_into().unwrap_or(i64::MAX),0).expect("Timestamp is within range"),
                            ByteSize::b(size).display().si()
                        ));
                    } else {
                        message.push_str("\nIs stored: false");
                    }

                    message.push_str(&format!("\nIs accessible via unauthenticated media endpoints: {unauthenticated_access_permitted}"));
                    message.push_str(&format!("\nSHA256 hash of file: {sha256_hex}"));
                    message.push_str(&format!("\nIs blocked due to sharing SHA256 hash with blocked media: {is_blocked_via_filehash}"));

                    if let Some(filename) = filename {
                        message.push_str(&format!("\nFilename: {filename}"))
                    }
                    if let Some(content_type) = content_type {
                        message.push_str(&format!("\nContent-type: {content_type}"))
                    }
                }

                message.into()
            }
            Command::ShowMedia { mxc } => {
                let Ok((server_name, media_id)) = mxc.parts() else {
                    return Ok("Invalid media MXC".into());
                };

                // TODO: Bypass blocking once MSC3911 is implemented (linking media to events)
                let ruma::api::client::authenticated_media::get_content::v1::Response {
                    file,
                    content_type,
                    content_disposition,
                } = client_server::media::get_content(
                    server_name,
                    media_id.to_owned(),
                    Target::User(services().globals.server_user().to_owned()),
                )
                .await?;

                if let Ok(image) = image::load_from_memory(&file) {
                    let filename = content_disposition.and_then(|cd| cd.filename);
                    let (width, height) = image.dimensions();

                    CommandOutput::Image(ImageMessageEventContent {
                        body: filename.clone().unwrap_or_default(),
                        formatted: None,
                        filename,
                        source: MediaSource::Plain(mxc.to_owned()),
                        info: Some(Box::new(ruma::events::room::ImageInfo {
                            height: Some(height.into()),
                            width: Some(width.into()),
                            mimetype: content_type,
                            size: size(&file)?.try_into().ok(),
                            thumbnail_info: None,
                            thumbnail_source: None,
                            blurhash: None,
                            thumbhash: None,
                            is_animated: None,
                        })),
                    })
                } else {
                    let filename = content_disposition.and_then(|cd| cd.filename);

                    CommandOutput::File(FileMessageEventContent {
                        body: filename.clone().unwrap_or_default(),
                        formatted: None,
                        filename,
                        source: MediaSource::Plain(mxc.to_owned()),
                        info: Some(Box::new(ruma::events::room::message::FileInfo {
                            mimetype: content_type,
                            size: size(&file)?.try_into().ok(),
                            thumbnail_info: None,
                            thumbnail_source: None,
                        })),
                    })
                }
            }
            Command::ListMedia {
                user_server_filter: ListMediaArgs { user, server },
                include_thumbnails,
                content_type,
                uploaded_before,
                uploaded_after,
                page,
            } => CommandOutput::Table {
                title: "Stored Media",
                headers: &[
                    "MXC URI",
                    "Dimensions (if thumbnail)",
                    "Created/Downloaded at",
                    "Uploader",
                    "Content-Type",
                    "Filename",
                    "Size",
                ],
                page,
                rows: services()
                    .media
                    .list(
                        user.map(ServerNameOrUserId::UserId)
                            .or_else(|| server.map(ServerNameOrUserId::ServerName)),
                        include_thumbnails,
                        content_type.as_deref(),
                        uploaded_before
                            .map(|ts| ts.duration_since(UNIX_EPOCH))
                            .transpose()
                            .map_err(|_| Error::Plain("Timestamp must be after unix epoch".into()))?
                            .as_ref()
                            .map(Duration::as_secs),
                        uploaded_after
                            .map(|ts| ts.duration_since(UNIX_EPOCH))
                            .transpose()
                            .map_err(|_| Error::Plain("Timestamp must be after unix epoch".into()))?
                            .as_ref()
                            .map(Duration::as_secs),
                        PAGE_SIZE,
                        (page.get() - 1) * PAGE_SIZE,
                    )?
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            },
            Command::PurgeMedia { mxc_uris } => {
                let media = mxc_uris_to_parts(mxc_uris)?;

                let failed_count = services().media.purge(&media, true).await.len();

                if failed_count == 0 {
                    "Successfully purged media".into()
                } else {
                    return Err(Error::plain(format!(
                        "Failed to delete {failed_count} media, check logs for more details"
                    )));
                }
            }
            Command::PurgeMediaFromUsers {
                from_last,
                force_filehash,
                user_ids,
            } => {
                let after = from_last.map(unix_secs_from_duration).transpose()?;

                let mut failed_count = 0;

                for user_id in user_ids {
                    failed_count += services()
                        .media
                        .purge_from_user(&user_id, force_filehash, after)
                        .await
                        .len();
                }

                if failed_count == 0 {
                    "Successfully purged media".into()
                } else {
                    return Err(Error::plain(format!(
                        "Failed to purge {failed_count} media, check logs for more details"
                    )));
                }
            }
            Command::PurgeMediaFromServer {
                server_id: server_name,
                from_last,
                force_filehash,
            } => {
                if server_name == services().globals.server_name() {
                    return Err(Error::plain(
                        "Cannot purge all media from your own homeserver",
                    ));
                }

                let after = from_last.map(unix_secs_from_duration).transpose()?;

                let failed_count = services()
                    .media
                    .purge_from_server(&server_name, force_filehash, after)
                    .await
                    .len();

                if failed_count == 0 {
                    CommandOutput::Plain(format!(
                        "Media from {server_name} has successfully been purged"
                    ))
                } else {
                    return Err(Error::plain(format!(
                        "Failed to purge {failed_count} media, check logs for more details"
                    )));
                }
            }
            Command::BlockMedia {
                and_purge,
                reason,
                mxc_uris,
            } => {
                let media = mxc_uris_to_parts(mxc_uris)?;

                let failed_count = services().media.block(&media, reason).len();
                let failed_purge_count = if and_purge {
                    services().media.purge(&media, true).await.len()
                } else {
                    0
                };

                match (failed_count == 0, failed_purge_count == 0) {
                    (true, true) => "Successfully blocked media".into(),
                    (false, true) => {
                        return Err(Error::plain(format!(
                            "Failed to block {failed_count} media, check logs for more details"
                        )));
                    }
                    (true, false) => {
                        return Err(Error::plain(format!(
                            "Failed to purge {failed_purge_count} media, check logs for more details"
                        )));
                    }
                    (false, false) => {
                        return Err(Error::plain(format!(
                            "Failed to block {failed_count}, and purge {failed_purge_count} media, check logs for more details"
                        )));
                    }
                }
            }
            Command::BlockMediaFromUsers {
                from_last,
                reason,
                user_ids,
            } => {
                let after = from_last.map(unix_secs_from_duration).transpose()?;

                let mut failed_count = 0;

                for user_id in user_ids {
                    let reason = reason.as_ref().map_or_else(
                        || Cow::Owned(format!("uploaded by {user_id}")),
                        Cow::Borrowed,
                    );

                    failed_count += services()
                        .media
                        .block_from_user(&user_id, &reason, after)
                        .len();
                }

                if failed_count == 0 {
                    "Successfully blocked media".into()
                } else {
                    return Err(Error::plain(format!(
                        "Failed to block {failed_count} media, check logs for more details"
                    )));
                }
            }
            Command::ListBlockedMedia { page } => CommandOutput::Table {
                title: "Blocked media",
                headers: &["SHA256 hash", "MXC URI", "Blocked At", "Reason"],
                page,
                rows: services()
                    .media
                    .list_blocked(PAGE_SIZE, (page.get() - 1) * PAGE_SIZE)?
                    .into_iter()
                    .map(Into::into)
                    .collect(),
            },
            Command::UnblockMedia { mxc_uris } => {
                let media = mxc_uris_to_parts(mxc_uris)?;

                let failed_count = services().media.unblock(&media).len();

                if failed_count == 0 {
                    "Successfully unblocked media".into()
                } else {
                    return Err(Error::plain(format!(
                        "Failed to unblock {failed_count} media, check logs for more details"
                    )));
                }
            }
            Command::SignJson => {
                let Some(input) = text_input else {
                    return Err(Error::MissingInput.into());
                };

                match serde_json::from_str(&input) {
                    Ok(mut value) => {
                        ruma::signatures::sign_json(
                            services().globals.server_name().as_str(),
                            services().globals.keypair(),
                            &mut value,
                        )
                        .expect("our request json is what ruma expects");
                        let json_text = serde_json::to_string_pretty(&value)
                            .expect("canonical json is valid json");
                        CommandOutput::Plain(json_text)
                    }
                    Err(e) => CommandOutput::Plain(format!("Invalid json: {e}")),
                }
            }
            Command::VerifyJson => {
                let Some(input) = text_input else {
                    return Err(Error::MissingInput.into());
                };
                match serde_json::from_str(&input) {
                    Ok(value) => {
                        let pub_key_map = RwLock::new(BTreeMap::new());

                        services()
                            .rooms
                            .event_handler
                            // Generally we shouldn't be checking against expired keys unless required, so in the admin
                            // room it might be best to not allow expired keys
                            .fetch_required_signing_keys(&value, &pub_key_map)
                            .await?;

                        let mut expired_key_map = BTreeMap::new();
                        let mut valid_key_map = BTreeMap::new();

                        for (server, keys) in pub_key_map.into_inner().into_iter().chain(vec![(
                            services().globals.server_name().to_string(),
                            SigningKeys::load_own_keys(),
                        )]) {
                            if keys.valid_until_ts > MilliSecondsSinceUnixEpoch::now() {
                                valid_key_map.insert(
                                    server.clone(),
                                    keys.verify_keys
                                        .clone()
                                        .into_iter()
                                        .map(|(id, key)| (id, key.key))
                                        .collect(),
                                );
                            }
                            // Valid keys can go into the expired key map
                            expired_key_map.insert(
                                server,
                                keys.verify_keys
                                    .into_iter()
                                    .map(|(id, key)| (id, key.key))
                                    .collect(),
                            );
                        }

                        if ruma::signatures::verify_json(&valid_key_map, &value).is_ok() {
                            "Signature correct".into()
                        } else if let Err(e) =
                            ruma::signatures::verify_json(&expired_key_map, &value)
                        {
                            return Err(Error::plain(format!(
                                "Signature verification failed: {e}"
                            )));
                        } else {
                            return Err(Error::plain("Signature correct (with expired keys)"));
                        }
                    }
                    Err(e) => return Err(Error::plain(format!("Invalid json: {e}"))),
                }
            }
            Command::HashAndSignEvent { room_version_id } => {
                let Some(input) = text_input else {
                    return Err(Error::MissingInput.into());
                };
                match serde_json::from_str(&input) {
                    Ok(mut value) => {
                        if let Err(e) = ruma::signatures::hash_and_sign_event(
                            services().globals.server_name().as_str(),
                            services().globals.keypair(),
                            &mut value,
                            &room_version_id
                                .rules()
                                .expect("Supported room version has rules")
                                .redaction,
                        ) {
                            return Err(Error::plain(format!("Invalid event: {e}")));
                        } else {
                            let json_text = serde_json::to_string_pretty(&value)
                                .expect("canonical json is valid json");
                            json_text.into()
                        }
                    }
                    Err(e) => return Err(Error::plain(format!("Invalid json: {e}"))),
                }
            }
            Command::RemoveAlias { alias } => {
                if alias.server_name() != services().globals.server_name() {
                    return Err(Error::plain(
                        "Cannot remove alias which is not from this server",
                    ));
                } else if services()
                    .rooms
                    .alias
                    .resolve_local_alias(&alias)?
                    .is_none()
                {
                    return Err(Error::plain("No such alias exists"));
                } else {
                    // We execute this as the server user for two reasons
                    // 1. If the user can execute commands in the admin room, they can always remove the alias.
                    // 2. In the future, we are likely going to be able to allow users to execute commands via
                    //   other methods, such as IPC, which would lead to us not knowing their user id

                    services()
                        .rooms
                        .alias
                        .remove_alias(&alias, services().globals.server_user())?;
                    "Alias removed successfully".into()
                }
            }
        })
    }
}

fn mxc_uris_to_parts(mxc_uris: Vec<OwnedMxcUri>) -> Result<Vec<(OwnedServerName, String)>> {
    mxc_uris
        .into_iter()
        .map(|mxc_uri| {
            mxc_uri
                .parts()
                .map_err(|e| Error::plain(format!("Failed to parse MXC Uri {mxc_uri}: {e}")))
                .map(|(server, id)| (server.to_owned(), id.to_owned()))
        })
        .collect::<Result<Vec<_>>>()
}

fn unix_secs_from_duration(duration: Duration) -> Result<u64> {
    SystemTime::now()
        .checked_sub(duration).ok_or_else(||Error::plain("Given timeframe cannot be represented as system time, please try again with a shorter time-frame"))
        .map(|time| time
                .duration_since(UNIX_EPOCH)
                .expect("Time is after unix epoch")
                .as_secs())
}
