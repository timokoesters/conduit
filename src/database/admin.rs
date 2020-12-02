use std::convert::{TryFrom, TryInto};

use crate::pdu::PduBuilder;
use log::warn;
use rocket::futures::{channel::mpsc, stream::StreamExt};
use ruma::{events::room::message, events::EventType, UserId};
use tokio::select;

pub enum AdminCommand {
    SendTextMessage(message::TextMessageEventContent),
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
                warn!("Conduit instance does not have an #admins room. Logging to that room will not work.");
            }

            loop {
                select! {
                    Some(event) = receiver.next() => {
                        match event {
                            AdminCommand::SendTextMessage(message) => {
                                println!("{:?}", message);

                                if let Some(conduit_room) = &conduit_room {
                                    db.rooms.build_and_append_pdu(
                                        PduBuilder {
                                            event_type: EventType::RoomMessage,
                                            content: serde_json::to_value(message).expect("event is valid, we just created it"),
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
                                    ).unwrap();
                                }
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
