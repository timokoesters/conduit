use std::convert::{TryFrom, TryInto};

use crate::pdu::PduBuilder;
use log::warn;
use rocket::futures::{channel::mpsc, stream::StreamExt};
use ruma::{
    events::{room::message, EventType},
    UserId,
};

pub enum AdminCommand {
    RegisterAppservice(serde_yaml::Value),
    ListAppservices,
    SendMessage(message::MessageEventContent),
}

#[derive(Clone)]
pub struct Admin {
    pub sender: mpsc::UnboundedSender<AdminCommand>,
}

impl Admin {
    pub fn start_handler(
        &self,
        db: super::Database,
        mut receiver: mpsc::UnboundedReceiver<AdminCommand>,
    ) {
        tokio::spawn(async move {
            // TODO: Use futures when we have long admin commands
            //let mut futures = FuturesUnordered::new();

            let conduit_user = UserId::try_from(format!("@conduit:{}", db.globals.server_name()))
                .expect("@conduit:server_name is valid");

            let conduit_room = db
                .rooms
                .id_from_alias(
                    &format!("#admins:{}", db.globals.server_name())
                        .try_into()
                        .expect("#admins:server_name is a valid room alias"),
                )
                .unwrap();

            if conduit_room.is_none() {
                warn!("Conduit instance does not have an #admins room. Logging to that room will not work. Restart Conduit after creating a user to fix this.");
            }

            let send_message = |message: message::MessageEventContent| {
                if let Some(conduit_room) = &conduit_room {
                    db.rooms
                        .build_and_append_pdu(
                            PduBuilder {
                                event_type: EventType::RoomMessage,
                                content: serde_json::to_value(message)
                                    .expect("event is valid, we just created it"),
                                unsigned: None,
                                state_key: None,
                                redacts: None,
                            },
                            &conduit_user,
                            &conduit_room,
                            &db.globals,
                            &db.sending,
                            &db.admin,
                            &db.account_data,
                            &db.appservice,
                        )
                        .unwrap();
                }
            };

            loop {
                tokio::select! {
                    Some(event) = receiver.next() => {
                        match event {
                            AdminCommand::RegisterAppservice(yaml) => {
                                db.appservice.register_appservice(yaml).unwrap(); // TODO handle error
                            }
                            AdminCommand::ListAppservices => {
                                let appservices = db.appservice.iter_ids().collect::<Vec<_>>();
                                let count = appservices.len();
                                let output = format!(
                                    "Appservices ({}): {}",
                                    count,
                                    appservices.into_iter().filter_map(|r| r.ok()).collect::<Vec<_>>().join(", ")
                                );
                                send_message(message::MessageEventContent::text_plain(output));
                            }
                            AdminCommand::SendMessage(message) => {
                                send_message(message);
                            }
                        }
                    }
                }
            }
        });
    }

    pub fn send(&self, command: AdminCommand) {
        self.sender.unbounded_send(command).unwrap()
    }
}
