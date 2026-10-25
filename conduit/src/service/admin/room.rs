use std::{collections::BTreeMap, sync::Arc};

use clap::Parser;
use regex::Regex;
use ruma::{
    OwnedRoomAliasId, OwnedRoomId, ServerName, UserId,
    events::{
        TimelineEventType,
        room::{
            canonical_alias::RoomCanonicalAliasEventContent,
            create::RoomCreateEventContent,
            guest_access::{GuestAccess, RoomGuestAccessEventContent},
            history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            member::{MembershipState, RoomMemberEventContent},
            message::{MessageType, RoomMessageEventContent},
            name::RoomNameEventContent,
            power_levels::RoomPowerLevelsEventContent,
            topic::RoomTopicEventContent,
        },
    },
};
use serde_json::value::to_raw_value;
use tokio::sync::{Mutex, mpsc};

use crate::{Result, service::pdu::PduBuilder, services};

use super::Command;

#[cfg_attr(test, derive(Debug))]
#[derive(Parser)]
#[command(version = env!("CARGO_PKG_VERSION"))]
struct Args {
    #[command(subcommand)]
    pub command: Command,
}

pub trait Output {
    fn to_message(self) -> MessageType;
}

impl Output for super::CommandOutput {
    fn to_message(self) -> MessageType {
        match self {
            Self::Plain(message) => RoomMessageEventContent::text_plain(message).into(),
            Self::TaskComplete(message) => RoomMessageEventContent::text_plain(message).into(),
            Self::Image(image_event_content) => MessageType::Image(image_event_content),
            Self::File(file_event_content) => MessageType::File(file_event_content),
            Self::Table {
                title,
                headers,
                page,
                rows,
            } => if rows.is_empty() {
                RoomMessageEventContent::text_plain(format!("{title} has no page {page}"))
            } else {
                let markdown_message = format!("# {title} - page {page}:\n| ")
                    + &headers.join(" | ")
                    + " |\n| "
                    + &rows
                        .clone()
                        .into_iter()
                        .map(|row| {
                            row.get()
                                .into_iter()
                                .map(markdown_rowitem)
                                .collect::<Vec<_>>()
                                .join(" | ")
                        })
                        .collect::<Vec<_>>()
                        .join(" |\n")
                    + " |";

                let html_message = format!(
                    "<table><caption>{title} - page {page}</caption><thead><tr><th scope=\"col\">"
                ) + &headers.join("</th><th scope=\"col\">")
                    + "</th></tr></thead><tbody><tr><td>"
                    + &rows
                        .into_iter()
                        .map(|row| {
                            row.get()
                                .into_iter()
                                .map(html_rowitem)
                                .collect::<Vec<_>>()
                                .join("</td><td>")
                        })
                        .collect::<Vec<_>>()
                        .join("</td></tr><tr><td>")
                    + "</td></tr></tbody></table>";

                RoomMessageEventContent::text_html(markdown_message, html_message)
            }
            .into(),
            Self::CodeBlockWithMsg {
                code_block: super::CodeBlock { code, language },
                message,
            } => {
                let markdown_message = format!("{message}\n```{language}\n{code}\n```");
                let html_message = format!(
                    "<p>{message}</p>\n<pre><code class=\"language-{language}\">{code}\n</code></pre>"
                );

                RoomMessageEventContent::text_html(markdown_message, html_message)
            }
            .into(),
        }
    }
}

fn markdown_rowitem(value: super::RowItem) -> String {
    match value {
        super::RowItem::Plain(value) => value,
        super::RowItem::Code(value) => format!("`{value}`"),
        super::RowItem::Empty => String::new(),
    }
}

fn html_rowitem(value: super::RowItem) -> String {
    match value {
        super::RowItem::Plain(value) => value,
        super::RowItem::Code(value) => format!("<code>{value}</code>"),
        super::RowItem::Empty => String::new(),
    }
}

#[derive(Debug)]
pub enum AdminRoomEvent {
    ProcessMessage(String),
    SendMessage(Box<RoomMessageEventContent>),
}

pub struct Service {
    pub sender: mpsc::UnboundedSender<AdminRoomEvent>,
    receiver: Mutex<mpsc::UnboundedReceiver<AdminRoomEvent>>,
}

impl Service {
    pub fn build() -> Arc<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();
        Arc::new(Self {
            sender,
            receiver: Mutex::new(receiver),
        })
    }

    pub fn start_handler(self: &Arc<Self>) {
        let self2 = Arc::clone(self);
        tokio::spawn(async move {
            self2.handler().await;
        });
    }

    async fn handler(&self) {
        let mut receiver = self.receiver.lock().await;
        // TODO: Use futures when we have long admin commands
        //let mut futures = FuturesUnordered::new();

        let conduit_user = services().globals.server_user();

        if let Ok(Some(conduit_room)) = services().admin.room.get_id() {
            loop {
                tokio::select! {
                    Some(event) = receiver.recv() => {
                        let message_content = match event {
                            AdminRoomEvent::SendMessage(content) => (*content).into(),
                            AdminRoomEvent::ProcessMessage(room_message) => self.process_admin_message(room_message).await,
                        };

                        let mutex_state = Arc::clone(
                            services().globals
                                .roomid_mutex_state
                                .write()
                                .await
                                .entry(conduit_room.to_owned())
                                .or_default(),
                        );

                        let state_lock = mutex_state.lock().await;

                        services()
                            .rooms
                            .timeline
                            .build_and_append_pdu(
                                PduBuilder {
                                    event_type: TimelineEventType::RoomMessage,
                                    content: to_raw_value(&message_content)
                                        .expect("event is valid, we just created it"),
                                    unsigned: None,
                                    state_key: None,
                                    redacts: None,
                                    timestamp: None,
                                },
                                conduit_user,
                                &conduit_room,
                                &state_lock,
                            )
                            .await.unwrap();
                    }
                }
            }
        }
    }

    pub fn process_message(&self, room_message: String) {
        self.sender
            .send(AdminRoomEvent::ProcessMessage(room_message))
            .unwrap();
    }

    pub fn send_message(&self, message_content: RoomMessageEventContent) {
        self.sender
            .send(AdminRoomEvent::SendMessage(Box::new(message_content)))
            .unwrap();
    }

    // Parse and process a message from the admin room
    async fn process_admin_message(&self, room_message: String) -> MessageType {
        let mut lines = room_message.lines().filter(|l| !l.trim().is_empty());
        let command_line = lines.next().expect("each string has at least one line");
        let body: Vec<_> = lines.collect();

        let admin_command = match self.parse_admin_command(command_line) {
            Ok(command) => command,
            Err(error) => {
                let server_name = services().globals.server_name();
                let message = error.replace("server.name", server_name.as_str());
                let html_message = self.usage_to_html(&message, server_name);

                return RoomMessageEventContent::text_html(message, html_message).into();
            }
        };

        match self.process_command(admin_command, body).await {
            Ok(reply_message) => reply_message,
            Err(error) => {
                let markdown_message = format!(
                    "Encountered an error while handling the command:\n\
                    ```\n{error}\n```",
                );
                let html_message = format!(
                    "Encountered an error while handling the command:\n\
                    <pre>\n{error}\n</pre>",
                );

                RoomMessageEventContent::text_html(markdown_message, html_message).into()
            }
        }
    }

    // Parse chat messages from the admin room into an AdminCommand object
    fn parse_admin_command(&self, command_line: &str) -> std::result::Result<Command, String> {
        // Note: argv[0] is `@conduit:servername:`, which is treated as the main command
        let mut argv = match shell_words::split(command_line) {
            Ok(args) => args,
            Err(e) => return Err(format!("Failed to parse admin command: {e}")),
        };

        // Replace `help command` with `command --help`
        // Clap has a help subcommand, but it omits the long help description.
        if argv.len() > 1 && argv[1] == "help" {
            argv.remove(1);
            argv.push("--help".to_owned());
        }

        // Backwards compatibility with `register_appservice`-style commands
        if let Some(command) = argv.get_mut(1)
            && command.contains('_')
        {
            *command = command.replace('_', "-");
        }

        Args::try_parse_from(&argv)
            .map_err(|error| error.to_string())
            .map(|args| args.command)
    }

    async fn process_command(&self, command: Command, body: Vec<&str>) -> Result<MessageType> {
        let code_block_input =
            if body.len() > 2 && body[0].trim() == "```" && body.last().unwrap().trim() == "```" {
                Some(body[1..body.len() - 1].join("\n"))
            } else {
                None
            };

        services()
            .admin
            .process_command(command, code_block_input)
            .await
            .map(Output::to_message)
    }

    // Utility to turn clap's `--help` text to HTML.
    fn usage_to_html(&self, text: &str, server_name: &ServerName) -> String {
        // Replace `@conduit:servername:-subcmdname` with `@conduit:servername: subcmdname`
        let text = text.replace(
            &format!("@conduit:{server_name}:-"),
            &format!("@conduit:{server_name}: "),
        );

        // For the conduit admin room, subcommands become main commands
        let text = text.replace("SUBCOMMAND", "COMMAND");
        let text = text.replace("subcommand", "command");

        // Escape option names (e.g. `<element-id>`) since they look like HTML tags
        let text = text.replace('<', "&lt;").replace('>', "&gt;");

        // Italicize the first line (command name and version text)
        let re = Regex::new("^(.*?)\n").expect("Regex compilation should not fail");
        let text = re.replace_all(&text, "<em>$1</em>\n");

        // Unmerge wrapped lines
        let text = text.replace("\n            ", "  ");

        // Wrap option names in backticks. The lines look like:
        //     -V, --version  Prints version information
        // And are converted to:
        // <code>-V, --version</code>: Prints version information
        // (?m) enables multi-line mode for ^ and $
        let re = Regex::new("(?m)^    (([a-zA-Z_&;-]+(, )?)+)  +(.*)$")
            .expect("Regex compilation should not fail");
        let text = re.replace_all(&text, "<code>$1</code>: $4");

        // Look for a `[commandbody]()` tag. If it exists, use all lines below it that
        // start with a `#` in the USAGE section.
        let mut text_lines: Vec<&str> = text.lines().collect();
        let mut command_body = String::new();

        if let Some(line_index) = text_lines
            .iter()
            .position(|line| *line == "[commandbody]()")
        {
            text_lines.remove(line_index);

            while text_lines
                .get(line_index)
                .map(|line| line.starts_with('#'))
                .unwrap_or(false)
            {
                command_body += if text_lines[line_index].starts_with("# ") {
                    &text_lines[line_index][2..]
                } else {
                    &text_lines[line_index][1..]
                };
                command_body += "[nobr]\n";
                text_lines.remove(line_index);
            }
        }

        let text = text_lines.join("\n");

        // Improve the usage section
        let text = if command_body.is_empty() {
            // Wrap the usage line in code tags
            let re = Regex::new("(?m)^USAGE:\n    (@conduit:.*)$")
                .expect("Regex compilation should not fail");
            re.replace_all(&text, "USAGE:\n<code>$1</code>").to_string()
        } else {
            // Wrap the usage line in a code block, and add a yaml block example
            // This makes the usage of e.g. `register-appservice` more accurate
            let re = Regex::new("(?m)^USAGE:\n    (.*?)\n\n")
                .expect("Regex compilation should not fail");
            re.replace_all(&text, "USAGE:\n<pre>$1[nobr]\n[commandbodyblock]</pre>")
                .replace("[commandbodyblock]", &command_body)
        };

        // Add HTML line-breaks

        text.replace("\n\n\n", "\n\n")
            .replace('\n', "<br>\n")
            .replace("[nobr]<br>", "")
    }

    /// Create the admin room.
    ///
    /// Users in this room are considered admins by conduit, and the room can be
    /// used to issue admin commands by talking to the server user inside it.
    pub async fn create(&self) -> Result<()> {
        // Create a user for the server
        let conduit_user = services().globals.server_user();

        services().users.create(conduit_user, None)?;

        let room_version = services().globals.default_room_version();
        let rules = room_version
            .rules()
            .expect("Supported room version must have rules.")
            .authorization;
        let mut content = if rules.use_room_create_sender {
            RoomCreateEventContent::new_v11()
        } else {
            RoomCreateEventContent::new_v1(conduit_user.to_owned())
        };
        content.federate = true;
        content.predecessor = None;
        content.room_version = room_version;

        // 1. The room create event
        let (room_id, mutex_state) = services()
            .rooms
            .timeline
            .send_create_room(
                to_raw_value(&content).expect("event is valid, we just created it"),
                conduit_user,
                &rules,
            )
            .await?;
        let state_lock = mutex_state.lock().await;

        // 2. Make conduit bot join
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomMember,
                    content: to_raw_value(&RoomMemberEventContent {
                        membership: MembershipState::Join,
                        displayname: None,
                        avatar_url: None,
                        is_direct: None,
                        third_party_invite: None,
                        blurhash: None,
                        reason: None,
                        join_authorized_via_users_server: None,
                    })
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some(conduit_user.to_string()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 3. Power levels
        let mut users = BTreeMap::new();
        if !rules.explicitly_privilege_room_creators {
            users.insert(conduit_user.to_owned(), 100.into());
        }

        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomPowerLevels,
                    content: to_raw_value(&RoomPowerLevelsEventContent {
                        users,
                        ..RoomPowerLevelsEventContent::new(&rules)
                    })
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 4.1 Join Rules
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomJoinRules,
                    content: to_raw_value(&RoomJoinRulesEventContent::new(JoinRule::Invite))
                        .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 4.2 History Visibility
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomHistoryVisibility,
                    content: to_raw_value(&RoomHistoryVisibilityEventContent::new(
                        HistoryVisibility::Shared,
                    ))
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 4.3 Guest Access
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomGuestAccess,
                    content: to_raw_value(&RoomGuestAccessEventContent::new(
                        GuestAccess::Forbidden,
                    ))
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 5. Events implied by name and topic
        let room_name = format!("{} Admin Room", services().globals.server_name());
        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomName,
                    content: to_raw_value(&RoomNameEventContent::new(room_name))
                        .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomTopic,
                    content: to_raw_value(&RoomTopicEventContent::new(format!(
                        "Manage {}",
                        services().globals.server_name()
                    )))
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        // 6. Room alias
        let alias: OwnedRoomAliasId = services().globals.admin_alias().to_owned();

        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomCanonicalAlias,
                    content: to_raw_value(&RoomCanonicalAliasEventContent {
                        alias: Some(alias.clone()),
                        alt_aliases: Vec::new(),
                    })
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some("".to_owned()),
                    redacts: None,
                    timestamp: None,
                },
                conduit_user,
                &room_id,
                &state_lock,
            )
            .await?;

        services()
            .rooms
            .alias
            .set_alias(&alias, &room_id, conduit_user)?;

        Ok(())
    }

    /// Gets the room ID of the admin room
    ///
    /// Errors are propagated from the database, and will have None if there is no admin room
    pub fn get_id(&self) -> Result<Option<OwnedRoomId>> {
        services()
            .rooms
            .alias
            .resolve_local_alias(services().globals.admin_alias())
    }

    /// Invite the user to the conduit admin room.
    ///
    /// They will only be able to run admin commands if they are configured to be an admin in the
    /// config file.
    pub async fn invite_user(&self, user_id: &UserId, displayname: String) -> Result<()> {
        if let Some(room_id) = self.get_id()? {
            let mutex_state = Arc::clone(
                services()
                    .globals
                    .roomid_mutex_state
                    .write()
                    .await
                    .entry(room_id.clone())
                    .or_default(),
            );
            let state_lock = mutex_state.lock().await;

            // Use the server user to grant the new admin's power level
            let conduit_user = services().globals.server_user();

            // Invite and join the real user
            services()
                .rooms
                .timeline
                .build_and_append_pdu(
                    PduBuilder {
                        event_type: TimelineEventType::RoomMember,
                        content: to_raw_value(&RoomMemberEventContent {
                            membership: MembershipState::Invite,
                            displayname: None,
                            avatar_url: None,
                            is_direct: None,
                            third_party_invite: None,
                            blurhash: None,
                            reason: None,
                            join_authorized_via_users_server: None,
                        })
                        .expect("event is valid, we just created it"),
                        unsigned: None,
                        state_key: Some(user_id.to_string()),
                        redacts: None,
                        timestamp: None,
                    },
                    conduit_user,
                    &room_id,
                    &state_lock,
                )
                .await?;
            services()
                .rooms
                .timeline
                .build_and_append_pdu(
                    PduBuilder {
                        event_type: TimelineEventType::RoomMember,
                        content: to_raw_value(&RoomMemberEventContent {
                            membership: MembershipState::Join,
                            displayname: Some(displayname),
                            avatar_url: None,
                            is_direct: None,
                            third_party_invite: None,
                            blurhash: None,
                            reason: None,
                            join_authorized_via_users_server: None,
                        })
                        .expect("event is valid, we just created it"),
                        unsigned: None,
                        state_key: Some(user_id.to_string()),
                        redacts: None,
                        timestamp: None,
                    },
                    user_id,
                    &room_id,
                    &state_lock,
                )
                .await?;

            // Set power level
            let room_version = services().rooms.state.get_room_version(&room_id)?;
            let rules = room_version
                .rules()
                .expect("Supported room version must have rules.")
                .authorization;

            let mut users = BTreeMap::new();
            if !rules.explicitly_privilege_room_creators {
                users.insert(conduit_user.to_owned(), 100.into());
            }
            users.insert(user_id.to_owned(), 100.into());

            services()
                .rooms
                .timeline
                .build_and_append_pdu(
                    PduBuilder {
                        event_type: TimelineEventType::RoomPowerLevels,
                        content: to_raw_value(&RoomPowerLevelsEventContent {
                            users,
                            ..RoomPowerLevelsEventContent::new(&rules)
                        })
                        .expect("event is valid, we just created it"),
                        unsigned: None,
                        state_key: Some("".to_owned()),
                        redacts: None,
                        timestamp: None,
                    },
                    conduit_user,
                    &room_id,
                    &state_lock,
                )
                .await?;

            // Send welcome message
            services().rooms.timeline.build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomMessage,
                content: to_raw_value(&RoomMessageEventContent::text_html(
                        format!("## Thank you for trying out Conduit!\n\nConduit is currently in Beta. This means you can join and participate in most Matrix rooms, but not all features are supported and you might run into bugs from time to time.\n\nHelpful links:\n> Website: https://conduit.rs\n> Git and Documentation: https://gitlab.com/famedly/conduit\n> Report issues: https://gitlab.com/famedly/conduit/-/issues\n\nFor a list of available commands, send the following message in this room: `@conduit:{}: --help`\n\nHere are some rooms you can join (by typing the command):\n\nConduit room (Ask questions and get notified on updates):\n`/join #conduit:ahimsa.chat`\n\nConduit lounge (Off-topic, only Conduit users are allowed to join)\n`/join #conduit-lounge:conduit.rs`", services().globals.server_name()),
                        format!("<h2>Thank you for trying out Conduit!</h2>\n<p>Conduit is currently in Beta. This means you can join and participate in most Matrix rooms, but not all features are supported and you might run into bugs from time to time.</p>\n<p>Helpful links:</p>\n<blockquote>\n<p>Website: https://conduit.rs<br>Git and Documentation: https://gitlab.com/famedly/conduit<br>Report issues: https://gitlab.com/famedly/conduit/-/issues</p>\n</blockquote>\n<p>For a list of available commands, send the following message in this room: <code>@conduit:{}: --help</code></p>\n<p>Here are some rooms you can join (by typing the command):</p>\n<p>Conduit room (Ask questions and get notified on updates):<br><code>/join #conduit:ahimsa.chat</code></p>\n<p>Conduit lounge (Off-topic, only Conduit users are allowed to join)<br><code>/join #conduit-lounge:conduit.rs</code></p>\n", services().globals.server_name()),
                ))
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: None,
                redacts: None,
                timestamp: None,
            },
            conduit_user,
            &room_id,
            &state_lock,
        ).await?;
        }
        Ok(())
    }

    /// Checks whether a given user is an admin of this server
    pub fn user_is_admin(&self, user_id: &UserId) -> Result<bool> {
        let Some(admin_room) = self.get_id()? else {
            return Ok(false);
        };

        services().rooms.state_cache.is_joined(user_id, &admin_room)
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn get_help_short() {
        get_help_inner("-h");
    }

    #[test]
    fn get_help_long() {
        get_help_inner("--help");
    }

    #[test]
    fn get_help_subcommand() {
        get_help_inner("help");
    }

    fn get_help_inner(input: &str) {
        let error = Args::try_parse_from(["argv[0] doesn't matter", input])
            .unwrap_err()
            .to_string();

        // Search for a handful of keywords that suggest the help printed properly
        assert!(error.contains("Usage:"));
        assert!(error.contains("Commands:"));
        assert!(error.contains("Options:"));
    }
}
