use std::{
    borrow::Cow,
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use bytesize::ByteSize;
use chrono::DateTime;
use conduit_common::{
    admin::{CodeBlock, Command, CommandOutput, ListMediaArgs, MediaFile},
    media::{self, FileInfo, ServerNameOrUserId},
};
use ruma::{
    EventId, MilliSecondsSinceUnixEpoch, OwnedMxcUri, OwnedServerName, RoomId, UserId,
    profile::ProfileFieldValue, room_version_rules::RoomVersionRules,
};
use thiserror::Error;
use tokio::{sync::RwLock, time::Instant};

use crate::{
    Error as ConduitError, PduEvent, Result,
    api::client_server::{self, AUTO_GEN_PASSWORD_LENGTH, leave_all_rooms},
    service::globals::SigningKeys,
    services, utils,
};

use super::rate_limiting::Target;

pub mod room;

static PAGE_SIZE: usize = 50;

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
                                language: "json".into(),
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

                CommandOutput::TaskComplete("Cleared database caches".into())
            }
            Command::ClearServiceCaches { amount } => {
                services().clear_caches(amount).await;

                CommandOutput::TaskComplete("Cleared service caches".into())
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
                CommandOutput::TaskComplete("Room disabled".into())
            }
            Command::EnableRoom { room_id } => {
                services().rooms.metadata.disable_room(&room_id, false)?;
                CommandOutput::TaskComplete("Room enabled".into())
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

                let media::Query {
                    is_blocked,
                    source_file,
                    thumbnails,
                } = services().media.query(server_name, media_id)?;
                let mut message = format!("Is blocked Media ID: {is_blocked}");

                if let Some(media::QueryFileInfo {
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

                for media::QueryThumbInfo {
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

                CommandOutput::File(MediaFile {
                    mxc,
                    file,
                    content_type,
                    filename: content_disposition.and_then(|cd| cd.filename),
                })
            }
            Command::ListMedia {
                user_server_filter: ListMediaArgs { user, server },
                include_thumbnails,
                content_type,
                uploaded_before,
                uploaded_after,
                page,
            } => CommandOutput::Table {
                title: "Stored Media".into(),
                headers: vec![
                    "MXC URI".into(),
                    "Dimensions (if thumbnail)".into(),
                    "Created/Downloaded at".into(),
                    "Uploader".into(),
                    "Content-Type".into(),
                    "Filename".into(),
                    "Size".into(),
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
                title: "Blocked media".into(),
                headers: vec![
                    "SHA256 hash".into(),
                    "MXC URI".into(),
                    "Blocked At".into(),
                    "Reason".into(),
                ],
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
