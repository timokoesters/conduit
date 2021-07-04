use std::{
    convert::{TryFrom, TryInto},
    sync::Arc,
};

use crate::{pdu::PduBuilder, Database};
use log::warn;
use rocket::futures::{channel::mpsc, stream::StreamExt};
use ruma::{
    events::{room::message, EventType},
    UserId,
};
use tokio::sync::{RwLock, RwLockReadGuard};

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
        db: Arc<RwLock<Database>>,
        mut receiver: mpsc::UnboundedReceiver<AdminCommand>,
    ) {
        tokio::spawn(async move {
            // TODO: Use futures when we have long admin commands
            //let mut futures = FuturesUnordered::new();

            let guard = db.read().await;

            let conduit_user =
                UserId::try_from(format!("@conduit:{}", guard.globals.server_name()))
                    .expect("@conduit:server_name is valid");

            let conduit_room = guard
                .rooms
                .id_from_alias(
                    &format!("#admins:{}", guard.globals.server_name())
                        .try_into()
                        .expect("#admins:server_name is a valid room alias"),
                )
                .unwrap();

            if conduit_room.is_none() {
                warn!("Conduit instance does not have an #admins room. Logging to that room will not work. Restart Conduit after creating a user to fix this.");
            }

            drop(guard);

            let send_message =
                |message: message::MessageEventContent, guard: RwLockReadGuard<'_, Database>| {
                    if let Some(conduit_room) = &conduit_room {
                        guard
                            .rooms
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
                                &guard,
                            )
                            .unwrap();
                    }
                };

            loop {
                tokio::select! {
                    Some(event) = receiver.next() => {
                        let guard = db.read().await;

                        match event {
                            AdminCommand::RegisterAppservice(yaml) => {
                                guard.appservice.register_appservice(yaml).unwrap(); // TODO handle error
                            }
                            AdminCommand::ListAppservices => {
                                if let Ok(appservices) = guard.appservice.iter_ids().map(|ids| ids.collect::<Vec<_>>()) {
                                    let count = appservices.len();
                                    let output = format!(
                                        "Appservices ({}): {}",
                                        count,
                                        appservices.into_iter().filter_map(|r| r.ok()).collect::<Vec<_>>().join(", ")
                                    );
                                    send_message(message::MessageEventContent::text_plain(output), guard);
                                } else {
                                    send_message(message::MessageEventContent::text_plain("Failed to get appservices."), guard);
                                }
                            }
                            AdminCommand::SendMessage(message) => {
                                send_message(message, guard)
                            }
                        }
                    }
                }
            }
        });
    }

    pub fn send(&self, command: AdminCommand) {
        self.sender.unbounded_send(command).unwrap();
    }
}
