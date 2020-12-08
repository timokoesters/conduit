use std::{collections::HashMap, convert::TryFrom, time::SystemTime};

use crate::{appservice_server, server_server, utils, Error, PduEvent, Result};
use federation::transactions::send_transaction_message;
use log::warn;
use rocket::futures::stream::{FuturesUnordered, StreamExt};
use ruma::{
    api::{appservice, federation},
    ServerName,
};
use sled::IVec;
use tokio::select;

#[derive(Clone)]
pub struct Sending {
    /// The state for a given state hash.
    pub(super) servernamepduids: sled::Tree, // ServernamePduId = (+)ServerName + PduId
    pub(super) servercurrentpdus: sled::Tree, // ServerCurrentPdus = (+)ServerName + PduId (pduid can be empty for reservation)
}

impl Sending {
    pub fn start_handler(
        &self,
        globals: &super::globals::Globals,
        rooms: &super::rooms::Rooms,
        appservice: &super::appservice::Appservice,
    ) {
        let servernamepduids = self.servernamepduids.clone();
        let servercurrentpdus = self.servercurrentpdus.clone();
        let rooms = rooms.clone();
        let globals = globals.clone();
        let appservice = appservice.clone();

        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();

            // Retry requests we could not finish yet
            let mut current_transactions = HashMap::new();

            for (server, pdu, is_appservice) in servercurrentpdus
                .iter()
                .filter_map(|r| r.ok())
                .map(|(key, _)| {
                    let mut parts = key.splitn(2, |&b| b == 0xff);
                    let server = parts.next().expect("splitn always returns one element");
                    let pdu = parts.next().ok_or_else(|| {
                        Error::bad_database("Invalid bytes in servercurrentpdus.")
                    })?;

                    let server = utils::string_from_bytes(&server).map_err(|_| {
                        Error::bad_database("Invalid server bytes in server_currenttransaction")
                    })?;

                    // Appservices start with a plus
                    let (server, is_appservice) = if server.starts_with("+") {
                        (&server[1..], true)
                    } else {
                        (&*server, false)
                    };

                    Ok::<_, Error>((
                        Box::<ServerName>::try_from(server).map_err(|_| {
                            Error::bad_database(
                                "Invalid server string in server_currenttransaction",
                            )
                        })?,
                        IVec::from(pdu),
                        is_appservice,
                    ))
                })
                .filter_map(|r| r.ok())
                .filter(|(_, pdu, _)| !pdu.is_empty()) // Skip reservation key
                .take(50)
            // This should not contain more than 50 anyway
            {
                current_transactions
                    .entry((server, is_appservice))
                    .or_insert_with(Vec::new)
                    .push(pdu);
            }

            for ((server, is_appservice), pdus) in current_transactions {
                futures.push(Self::handle_event(
                    server,
                    is_appservice,
                    pdus,
                    &globals,
                    &rooms,
                    &appservice,
                ));
            }

            let mut subscriber = servernamepduids.watch_prefix(b"");
            loop {
                select! {
                    Some(response) = futures.next() => {
                        match response {
                            Ok((server, is_appservice)) => {
                                let mut prefix = if is_appservice {
                                    "+".as_bytes().to_vec()
                                } else {
                                    Vec::new()
                                };
                                prefix.extend_from_slice(server.as_bytes());
                                prefix.push(0xff);

                                for key in servercurrentpdus
                                    .scan_prefix(&prefix)
                                    .keys()
                                    .filter_map(|r| r.ok())
                                {
                                    // Don't remove reservation yet
                                    if prefix.len() != key.len() {
                                        servercurrentpdus.remove(key).unwrap();
                                    }
                                }

                                // Find events that have been added since starting the last request
                                let new_pdus = servernamepduids
                                    .scan_prefix(&prefix)
                                    .keys()
                                    .filter_map(|r| r.ok())
                                    .map(|k| {
                                        k.subslice(prefix.len(), k.len() - prefix.len())
                                    })
                                    .take(50)
                                    .collect::<Vec<_>>();

                                if !new_pdus.is_empty() {
                                    for pdu_id in &new_pdus {
                                        let mut current_key = prefix.clone();
                                        current_key.extend_from_slice(pdu_id);
                                        servercurrentpdus.insert(&current_key, &[]).unwrap();
                                        servernamepduids.remove(&current_key).unwrap();
                                    }

                                    futures.push(Self::handle_event(server, is_appservice, new_pdus, &globals, &rooms, &appservice));
                                } else {
                                    servercurrentpdus.remove(&prefix).unwrap();
                                    // servercurrentpdus with the prefix should be empty now
                                }
                            }
                            Err((server, _is_appservice, e)) => {
                                warn!("Couldn't send transaction to {}: {}", server, e)
                                // TODO: exponential backoff
                            }
                        };
                    },
                    Some(event) = &mut subscriber => {
                        if let sled::Event::Insert { key, .. } = event {
                            let servernamepduid = key.clone();
                            let mut parts = servernamepduid.splitn(2, |&b| b == 0xff);

                            if let Some((server, is_appservice, pdu_id)) = utils::string_from_bytes(
                                    parts
                                        .next()
                                        .expect("splitn will always return 1 or more elements"),
                                )
                                .map_err(|_| Error::bad_database("ServerName in servernamepduid bytes are invalid."))
                                .map(|server_str| {
                                    // Appservices start with a plus
                                    if server_str.starts_with("+") {
                                        (server_str[1..].to_owned(), true)
                                    } else {
                                        (server_str, false)
                                    }
                                })
                                .and_then(|(server_str, is_appservice)| Box::<ServerName>::try_from(server_str)
                                    .map_err(|_| Error::bad_database("ServerName in servernamepduid is invalid.")).map(|s| (s, is_appservice)))
                                .ok()
                                .and_then(|(server, is_appservice)| parts
                                    .next()
                                    .ok_or_else(|| Error::bad_database("Invalid servernamepduid in db."))
                                    .ok()
                                    .map(|pdu_id| (server, is_appservice, pdu_id))
                                )
                                // TODO: exponential backoff
                                .filter(|(server, is_appservice, _)| {
                                    let mut prefix = if *is_appservice {
                                        "+".as_bytes().to_vec()
                                    } else {
                                        Vec::new()
                                    };
                                    prefix.extend_from_slice(server.as_bytes());
                                    prefix.push(0xff);

                                    servercurrentpdus
                                        .compare_and_swap(prefix, Option::<&[u8]>::None, Some(&[])) // Try to reserve
                                        == Ok(Ok(()))
                                })
                            {
                                servercurrentpdus.insert(&key, &[]).unwrap();
                                servernamepduids.remove(&key).unwrap();

                                futures.push(Self::handle_event(server, is_appservice, vec![pdu_id.into()], &globals, &rooms, &appservice));
                            }
                        }
                    }
                }
            }
        });
    }

    pub fn send_pdu(&self, server: &ServerName, pdu_id: &[u8]) -> Result<()> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    pub fn send_pdu_appservice(&self, appservice_id: &str, pdu_id: &[u8]) -> Result<()> {
        let mut key = "+".as_bytes().to_vec();
        key.extend_from_slice(appservice_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    async fn handle_event(
        server: Box<ServerName>,
        is_appservice: bool,
        pdu_ids: Vec<IVec>,
        globals: &super::globals::Globals,
        rooms: &super::rooms::Rooms,
        appservice: &super::appservice::Appservice,
    ) -> std::result::Result<(Box<ServerName>, bool), (Box<ServerName>, bool, Error)> {
        if is_appservice {
            let pdu_jsons = pdu_ids
                .iter()
                .map(|pdu_id| {
                    Ok::<_, (Box<ServerName>, Error)>(
                        rooms
                            .get_pdu_from_id(pdu_id)
                            .map_err(|e| (server.clone(), e))?
                            .ok_or_else(|| {
                                (
                                    server.clone(),
                                    Error::bad_database(
                                        "Event in servernamepduids not found in db.",
                                    ),
                                )
                            })?
                            .to_any_event(),
                    )
                })
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            appservice_server::send_request(
                &globals,
                appservice
                    .get_registration(server.as_str())
                    .unwrap()
                    .unwrap(), // TODO: handle error
                appservice::event::push_events::v1::Request {
                    events: &pdu_jsons,
                    txn_id: &utils::random_string(16),
                },
            )
            .await
            .map(|_response| (server.clone(), is_appservice))
            .map_err(|e| (server, is_appservice, e))
        } else {
            let pdu_jsons = pdu_ids
                .iter()
                .map(|pdu_id| {
                    Ok::<_, (Box<ServerName>, Error)>(
                        // TODO: check room version and remove event_id if needed
                        serde_json::from_str(
                            PduEvent::convert_to_outgoing_federation_event(
                                rooms
                                    .get_pdu_json_from_id(pdu_id)
                                    .map_err(|e| (server.clone(), e))?
                                    .ok_or_else(|| {
                                        (
                                            server.clone(),
                                            Error::bad_database(
                                                "Event in servernamepduids not found in db.",
                                            ),
                                        )
                                    })?,
                            )
                            .json()
                            .get(),
                        )
                        .expect("Raw<..> is always valid"),
                    )
                })
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();

            server_server::send_request(
                &globals,
                server.clone(),
                send_transaction_message::v1::Request {
                    origin: globals.server_name(),
                    pdus: &pdu_jsons,
                    edus: &[],
                    origin_server_ts: SystemTime::now(),
                    transaction_id: &utils::random_string(16),
                },
            )
            .await
            .map(|_response| (server.clone(), is_appservice))
            .map_err(|e| (server, is_appservice, e))
        }
    }
}
