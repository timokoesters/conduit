use std::{collections::HashSet, convert::TryFrom, sync::Mutex, time::SystemTime};

use crate::{server_server, utils, Error, PduEvent, Result};
use federation::transactions::send_transaction_message;
use log::debug;
use rocket::futures::stream::{FuturesUnordered, StreamExt};
use ruma::{api::federation, ServerName};
use sled::IVec;
use tokio::select;

pub struct Sending {
    /// The state for a given state hash.
    pub(super) serverpduids: sled::Tree, // ServerPduId = ServerName + PduId
    pub(super) server_currenttransaction: sled::Tree, // CurrentTransaction = Event1 - Event2  - ... (- = 0xfe)
}

impl Sending {
    pub fn start_handler(&self, globals: &super::globals::Globals, rooms: &super::rooms::Rooms) {
        let serverpduids = self.serverpduids.clone();
        let server_currenttransaction = self.server_currenttransaction.clone();
        let rooms = rooms.clone();
        let globals = globals.clone();

        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();
            let waiting_servers = Mutex::new(HashSet::new());

            // Retry requests we could finish last time
            for (server, events) in server_currenttransaction
                .iter()
                .filter_map(|r| r.ok())
                .map(|(server, events)| {
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
                        events,
                    ))
                })
                .filter_map(|r| r.ok())
            {
                waiting_servers.lock().unwrap().insert(server.clone());
                let pdus = events
                    .split(|&b| b == 0xfe)
                    .map(|event| event.into())
                    .collect::<Vec<_>>();
                futures.push(Self::handle_event(server, pdus, &globals, &rooms));
            }

            let mut subscriber = serverpduids.watch_prefix(b"");
            loop {
                select! {
                    Some(server) = futures.next() => {
                        debug!("response: {:?}", &server);
                        match server {
                            Ok((server, _response)) => {
                                for pdu_id in server_currenttransaction.remove(server.as_bytes())
                                    .unwrap()
                                    .expect("this can only be called if a transaction finishes")
                                    .split(|&b| b == 0xfe)
                                {
                                    let mut serverpduid = server.as_bytes().to_vec();
                                    serverpduid.push(0xff);
                                    serverpduid.extend_from_slice(&pdu_id);
                                    serverpduids.remove(&serverpduid).unwrap();
                                }

                                let mut prefix = server.as_bytes().to_vec();
                                prefix.push(0xff);

                                // Find events that have been added since starting the last request
                                let new_pdus = serverpduids
                                    .scan_prefix(&prefix)
                                    .keys()
                                    .filter_map(|r| r.ok())
                                    .map(|k| {
                                        k.subslice(prefix.len(), k.len() - prefix.len())
                                    }).collect::<Vec<_>>();

                                if !new_pdus.is_empty() {
                                    let transaction_id = new_pdus
                                        .iter()
                                        .map(|v| &**v)
                                        .collect::<Vec<&[u8]>>()
                                        .join(&[0xfe][..]);

                                    server_currenttransaction.insert(server.to_string(), transaction_id).unwrap();
                                    futures.push(Self::handle_event(server, new_pdus, &globals, &rooms));
                                } else {
                                    waiting_servers.lock().unwrap().remove(&server);
                                }
                            }
                            Err((server, _e)) => {
                            }
                        };
                    },
                    Some(event) = &mut subscriber => {
                        if let sled::Event::Insert { key, .. } = event {
                            let serverpduid = key.clone();
                            let mut parts = serverpduid.splitn(2, |&b| b == 0xff);

                            if let Some((server, pdu_id)) = utils::string_from_bytes(
                                    parts
                                        .next()
                                        .expect("splitn will always return 1 or more elements"),
                                )
                                .map_err(|_| Error::bad_database("ServerName in serverpduid bytes are invalid."))
                                .and_then(|server_str|Box::<ServerName>::try_from(server_str)
                                    .map_err(|_| Error::bad_database("ServerName in serverpduid is invalid.")))
                                .ok()
                                .filter(|server| waiting_servers.lock().unwrap().insert(server.clone())) // TODO: exponential backoff
                                .and_then(|server| parts
                                    .next()
                                    .ok_or_else(|| Error::bad_database("Invalid serverpduid in db."))
                                    .ok()
                                    .map(|pdu_id| (server, pdu_id))
                                )
                            {
                                // This should be empty, because if it is not, we are already
                                // waiting on the server to finish or we backed off
                                debug_assert!(server_currenttransaction.get(server.as_str()).unwrap().is_none());

                                server_currenttransaction.insert(server.to_string(), pdu_id).unwrap();
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
        self.serverpduids.insert(key, b"")?;

        Ok(())
    }

    async fn handle_event(
        server: Box<ServerName>,
        pdu_ids: Vec<IVec>,
        globals: &super::globals::Globals,
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
                                Error::bad_database("Event in serverpduids not found in db."),
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
