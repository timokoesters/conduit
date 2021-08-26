use std::{
    convert::{TryFrom, TryInto},
    sync::Arc,
};

use rocket::futures::{channel::mpsc, stream::StreamExt};
use ruma::{
    events::{EventType, room::message},
    UserId,
};
use tokio::sync::{MutexGuard, RwLock, RwLockWriteGuard};
use tracing::warn;

use crate::{Database, pdu::PduBuilder};

pub enum AdminCommand {
    RegisterAppservice(serde_yaml::Value),
    ListAppservices,
    SendMessage(message::MessageEventContent),
    ShowCacheUsage,
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

            let conduit_room = match conduit_room {
                None => {
                    warn!("Conduit instance does not have an #admins room. Logging to that room will not work. Restart Conduit after creating a user to fix this.");
                    return;
                }
                Some(r) => r,
            };

            drop(guard);

            let send_message = |message: message::MessageEventContent,
                                guard: RwLockWriteGuard<'_, Database>,
                                mutex_lock: &MutexGuard<'_, ()>| {
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
                        mutex_lock,
                    )
                    .unwrap();
            };

            loop {
                tokio::select! {
                    Some(event) = receiver.next() => {
                        let mut guard = db.write().await;
                        let mutex_state = Arc::clone(
                            guard.globals
                                .roomid_mutex_state
                                .write()
                                .unwrap()
                                .entry(conduit_room.clone())
                                .or_default(),
                        );

                        let state_lock = mutex_state.lock().await;

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
                                    send_message(message::MessageEventContent::text_plain(output), guard, &state_lock);
                                } else {
                                    send_message(message::MessageEventContent::text_plain("Failed to get appservices."), guard, &state_lock);
                                }
                            }
                            AdminCommand::SendMessage(message) => {
                                send_message(message, guard, &state_lock);
                            }
                            AdminCommand::ShowCacheUsage => {

                                fn format_cache_statistics_triple(name: String, triple: (usize, usize, usize)) -> String {
                                    let (memory_usage, item_count, capacity) = triple;
                                    format!(
                                        "{0} is using {1} MB ({2} bytes) of RAM at {3:.2}% utilization.",
                                        name,
                                        memory_usage / 100_00,
                                        memory_usage ,
                                        ((item_count as f32 / capacity as f32) * 100.0)
                                    )
                                }

                                if let Ok(cache_usage_statistics) = guard.get_cache_usage() {

                                    let mut statistics_lines =  Vec::with_capacity(7);
                                    statistics_lines.push(format_cache_statistics_triple("pdu_cache".to_string(), cache_usage_statistics.pdu_cache));
                                    statistics_lines.push(format_cache_statistics_triple("auth_chain_cache".to_string(), cache_usage_statistics.auth_chain_cache));
                                    statistics_lines.push(format_cache_statistics_triple("shorteventid_cache".to_string(), cache_usage_statistics.shorteventid_cache));
                                    statistics_lines.push(format_cache_statistics_triple("eventidshort_cache".to_string(), cache_usage_statistics.eventidshort_cache));
                                    statistics_lines.push(format_cache_statistics_triple("statekeyshort_cache".to_string(), cache_usage_statistics.statekeyshort_cache));
                                    statistics_lines.push(format_cache_statistics_triple("shortstatekey_cache".to_string(), cache_usage_statistics.shortstatekey_cache));
                                    statistics_lines.push(format_cache_statistics_triple("stateinfo_cache".to_string(), cache_usage_statistics.stateinfo_cache));

                                    send_message(message::MessageEventContent::text_plain(statistics_lines.join("\n")), guard, &state_lock);
                                } else {
                                    let result_text = "Could not calculate database cache size";
                                    send_message(message::MessageEventContent::text_plain(result_text), guard, &state_lock);
                                }

                            }
                        }

                        drop(state_lock);
                    }
                }
            }
        });
    }

    pub fn send(&self, command: AdminCommand) {
        self.sender.unbounded_send(command).unwrap();
    }
}
