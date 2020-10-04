use std::{collections::HashMap, convert::TryFrom, time::SystemTime};

use crate::{server_server, utils, Error, PduEvent, Result};
use federation::transactions::send_transaction_message;
use log::debug;
use rocket::futures::stream::{FuturesUnordered, StreamExt};
use ruma::{api::federation, ServerName};
use sled::IVec;
use tokio::select;

#[derive(Clone)]
pub struct Sending {
    /// The state for a given state hash.
    pub(super) servernamepduids: sled::Tree, // ServernamePduId = ServerName + PduId
    pub(super) servercurrentpdus: sled::Tree, // ServerCurrentPdus = ServerName + PduId (pduid can be empty for reservation)
}

impl Sending {
    pub fn start_handler(&self, globals: &super::globals::Globals<'static>, rooms: &super::rooms::Rooms) {
        let servernamepduids = self.servernamepduids.clone();
        let servercurrentpdus = self.servercurrentpdus.clone();
        let rooms = rooms.clone();
        let globals = globals.clone();

        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();

            // Retry requests we could not finish yet
            let mut current_transactions = HashMap::new();

            for (server, pdu) in servercurrentpdus
                .iter()
                .filter_map(|r| r.ok())
                .map(|(key, _)| {
                    let mut parts = key.splitn(2, |&b| b == 0xff);
                    let server = parts.next().expect("splitn always returns one element");
                    let pdu = parts.next().ok_or_else(|| {
                        Error::bad_database("Invalid bytes in servercurrentpdus.")
                    })?;

                    Ok::<_, Error>((
                        Box::<ServerName>::try_from(utils::string_from_bytes(&server).map_err(
                            |_| {
                                Error::bad_database(
                                    "Invalid server bytes in server_currenttransaction",
                                )
                            },
                        )?)
                        .map_err(|_| {
                            Error::bad_database(
                                "Invalid server string in server_currenttransaction",
                            )
                        })?,
                        IVec::from(pdu),
                    ))
                })
                .filter_map(|r| r.ok())
                .filter(|(_, pdu)| !pdu.is_empty()) // Skip reservation key
                .take(50)
            // This should not contain more than 50 anyway
            {
                current_transactions
                    .entry(server)
                    .or_insert_with(Vec::new)
                    .push(pdu);
            }

            for (server, pdus) in current_transactions {
                futures.push(Self::handle_event(server, pdus, &globals, &rooms));
            }

            let mut subscriber = servernamepduids.watch_prefix(b"");
            loop {
                select! {
                    Some(server) = futures.next() => {
                        debug!("response: {:?}", &server);
                        match server {
                            Ok((server, _response)) => {
                                let mut prefix = server.as_bytes().to_vec();
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

                                    futures.push(Self::handle_event(server, new_pdus, &globals, &rooms));
                                } else {
                                    servercurrentpdus.remove(&prefix).unwrap();
                                    // servercurrentpdus with the prefix should be empty now
                                }
                            }
                            Err((_server, _e)) => {
                                // TODO: exponential backoff
                            }
                        };
                    },
                    Some(event) = &mut subscriber => {
                        if let sled::Event::Insert { key, .. } = event {
                            let servernamepduid = key.clone();
                            let mut parts = servernamepduid.splitn(2, |&b| b == 0xff);

                            if let Some((server, pdu_id)) = utils::string_from_bytes(
                                    parts
                                        .next()
                                        .expect("splitn will always return 1 or more elements"),
                                )
                                .map_err(|_| Error::bad_database("ServerName in servernamepduid bytes are invalid."))
                                .and_then(|server_str|Box::<ServerName>::try_from(server_str)
                                    .map_err(|_| Error::bad_database("ServerName in servernamepduid is invalid.")))
                                .ok()
                                .and_then(|server| parts
                                    .next()
                                    .ok_or_else(|| Error::bad_database("Invalid servernamepduid in db."))
                                    .ok()
                                    .map(|pdu_id| (server, pdu_id))
                                )
                                // TODO: exponential backoff
                                .filter(|(server, _)| {
                                    let mut prefix = server.to_string().as_bytes().to_vec();
                                    prefix.push(0xff);

                                    servercurrentpdus
                                        .compare_and_swap(prefix, Option::<&[u8]>::None, Some(&[])) // Try to reserve
                                        == Ok(Ok(()))
                                })
                            {
                                servercurrentpdus.insert(&key, &[]).unwrap();
                                servernamepduids.remove(&key).unwrap();

                                futures.push(Self::handle_event(server, vec![pdu_id.into()], &globals, &rooms));
                            }
                        }
                    }
                }
            }
        });
    }

    pub fn send_pdu(&self, server: Box<ServerName>, pdu_id: &[u8]) -> Result<()> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    async fn handle_event(
        server: Box<ServerName>,
        pdu_ids: Vec<IVec>,
        globals: &super::globals::Globals<'static>,
        rooms: &super::rooms::Rooms,
    ) -> std::result::Result<
        (Box<ServerName>, send_transaction_message::v1::Response),
        (Box<ServerName>, Error),
    > {
        let pdu_jsons = pdu_ids
            .iter()
            .map(|pdu_id| {
                Ok::<_, (Box<ServerName>, Error)>(PduEvent::convert_to_outgoing_federation_event(
                    rooms
                        .get_pdu_json_from_id(pdu_id)
                        .map_err(|e| (server.clone(), e))?
                        .ok_or_else(|| {
                            (
                                server.clone(),
                                Error::bad_database("Event in servernamepduids not found in db."),
                            )
                        })?,
                ))
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
        .map(|response| (server.clone(), response))
        .map_err(|e| (server, e))
    }
}
