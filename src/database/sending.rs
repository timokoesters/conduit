use std::{collections::HashSet, convert::TryFrom, time::SystemTime};

use crate::{server_server, utils, Error, PduEvent, Result};
use federation::transactions::send_transaction_message;
use log::warn;
use rocket::futures::stream::{FuturesUnordered, StreamExt};
use ruma::{api::federation, ServerName};
use sled::IVec;
use tokio::select;

pub struct Sending {
    /// The state for a given state hash.
    pub(super) serverpduids: sled::Tree, // ServerPduId = ServerName + PduId
}

impl Sending {
    pub fn start_handler(&self, globals: &super::globals::Globals<'static>, rooms: &super::rooms::Rooms) {
        let serverpduids = self.serverpduids.clone();
        let rooms = rooms.clone();
        let globals = globals.clone();

        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();
            let mut waiting_servers = HashSet::new();

            let mut subscriber = serverpduids.watch_prefix(b"");
            loop {
                select! {
                    Some(server) = futures.next() => {
                        warn!("response: {:?}", &server);
                        warn!("futures left: {}", &futures.len());
                        match server {
                            Ok((server, _response)) => {
                                waiting_servers.remove(&server)
                            }
                            Err((server, _e)) => {
                                waiting_servers.remove(&server)
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
                                .filter(|server| waiting_servers.insert(server.clone()))
                                .and_then(|server| parts
                                .next()
                                .ok_or_else(|| Error::bad_database("Invalid serverpduid in db.")).ok().map(|pdu_id| (server, pdu_id)))
                            {
                                futures.push(Self::handle_event(server, pdu_id.into(), &globals, &rooms));
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
        pdu_id: IVec,
        globals: &super::globals::Globals<'static>,
        rooms: &super::rooms::Rooms,
    ) -> std::result::Result<
        (Box<ServerName>, send_transaction_message::v1::Response),
        (Box<ServerName>, Error),
    > {
        let pdu_json = PduEvent::convert_to_outgoing_federation_event(
            rooms
                .get_pdu_json_from_id(&pdu_id)
                .map_err(|e| (server.clone(), e))?
                .ok_or_else(|| {
                    (
                        server.clone(),
                        Error::bad_database("Event in serverpduids not found in db."),
                    )
                })?,
        );

        server_server::send_request(
            &globals,
            server.clone(),
            send_transaction_message::v1::Request {
                origin: globals.server_name(),
                pdus: &[pdu_json],
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
