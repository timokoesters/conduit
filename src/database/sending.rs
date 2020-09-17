use std::{convert::TryFrom, time::SystemTime};

use crate::{server_server, utils, Error, Result};
use rocket::futures::stream::{FuturesUnordered, StreamExt};
use ruma::{api::federation, Raw, ServerName};
use tokio::select;

pub struct Sending {
    /// The state for a given state hash.
    pub(super) serverpduids: sled::Tree, // ServerPduId = ServerName + PduId
}

impl Sending {
    pub fn start_handler(&self, globals: &super::globals::Globals, rooms: &super::rooms::Rooms) {
        let serverpduids = self.serverpduids.clone();
        let rooms = rooms.clone();
        let globals = globals.clone();

        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();
            let mut subscriber = serverpduids.watch_prefix(b"");
            loop {
                select! {
                    Some(_) = futures.next() => {},
                    Some(event) = &mut subscriber => {
                        let serverpduid = if let sled::Event::Insert {key, ..} = event {
                            key
                        } else { return Err::<(), Error>(Error::bad_database("")); };

                        let mut parts = serverpduid.splitn(2, |&b| b == 0xff);
                        let server = Box::<ServerName>::try_from(
                            utils::string_from_bytes(parts.next().expect("splitn will always return 1 or more elements"))
                                .map_err(|_| Error::bad_database("ServerName in serverpduid bytes are invalid."))?
                            ).map_err(|_| Error::bad_database("ServerName in serverpduid is invalid."))?;

                        let pdu_id = parts.next().ok_or_else(|| Error::bad_database("Invalid serverpduid in db."))?;
                        let mut pdu_json = rooms.get_pdu_json_from_id(&pdu_id.into())?.ok_or_else(|| Error::bad_database("Event in serverpduids not found in db."))?;

                        if let Some(unsigned) = pdu_json
                            .as_object_mut()
                            .expect("json is object")
                            .get_mut("unsigned") {
                                unsigned.as_object_mut().expect("unsigned is object").remove("transaction_id");
                        }

                        pdu_json
                            .as_object_mut()
                            .expect("json is object")
                            .remove("event_id");

                        let raw_json =
                            serde_json::from_value::<Raw<_>>(pdu_json).expect("Raw::from_value always works");

                        let globals = &globals;

                        futures.push(
                            async move {
                                let pdus = vec![raw_json];
                                let transaction_id = utils::random_string(16);

                                server_server::send_request(
                                    &globals,
                                    server,
                                    federation::transactions::send_transaction_message::v1::Request {
                                        origin: globals.server_name(),
                                        pdus: &pdus,
                                        edus: &[],
                                        origin_server_ts: SystemTime::now(),
                                        transaction_id: &transaction_id,
                                    },
                                ).await
                            }
                        );
                    },
                }
            }
        });
    }
    /*
     */

    pub fn send_pdu(&self, server: &ServerName, pdu_id: &[u8]) -> Result<()> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.serverpduids.insert(key, b"")?;

        Ok(())
    }
}
