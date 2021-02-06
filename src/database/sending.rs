use std::{
    collections::HashMap,
    convert::TryFrom,
    fmt::{Debug, Display, Formatter},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use crate::{appservice_server, server_server, utils, Database, Error, PduEvent, Result};
use federation::transactions::send_transaction_message;
use log::{error, info};
use rocket::futures::stream::{FuturesUnordered, StreamExt};
use ruma::{
    api::{appservice, federation, OutgoingRequest},
    events::{push_rules, EventType},
    uint, ServerName, UInt,
};
use sled::IVec;
use tokio::{select, sync::Semaphore};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutgoingKind {
    Appservice(Box<ServerName>),
    Push(Vec<u8>),
    Normal(Box<ServerName>),
}

impl Display for OutgoingKind {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            OutgoingKind::Appservice(name) => f.write_str(name.as_str()),
            OutgoingKind::Normal(name) => f.write_str(name.as_str()),
            OutgoingKind::Push(_) => f.write_str("Push notification TODO"),
        }
    }
}

#[derive(Clone)]
pub struct Sending {
    /// The state for a given state hash.
    pub(super) servernamepduids: sled::Tree, // ServernamePduId = (+ / $)ServerName / UserId + PduId
    pub(super) servercurrentpdus: sled::Tree, // ServerCurrentPdus = (+ / $)ServerName / UserId + PduId (pduid can be empty for reservation)
    pub(super) maximum_requests: Arc<Semaphore>,
}

impl Sending {
    pub fn start_handler(&self, db: &Database) {
        let servernamepduids = self.servernamepduids.clone();
        let servercurrentpdus = self.servercurrentpdus.clone();
        let db = db.clone();

        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();

            // Retry requests we could not finish yet
            let mut current_transactions = HashMap::new();

            for (outgoing_kind, pdu) in servercurrentpdus
                .iter()
                .filter_map(|r| r.ok())
                .filter_map(|(key, _)| Self::parse_servercurrentpdus(key).ok())
                .filter(|(_, pdu)| !pdu.is_empty()) // Skip reservation key
                .take(50)
            // This should not contain more than 50 anyway
            {
                current_transactions
                    .entry(outgoing_kind)
                    .or_insert_with(Vec::new)
                    .push(pdu);
            }

            for (outgoing_kind, pdus) in current_transactions {
                futures.push(Self::handle_event(outgoing_kind, pdus, &db));
            }

            let mut last_failed_try: HashMap<OutgoingKind, (u32, Instant)> = HashMap::new();

            let mut subscriber = servernamepduids.watch_prefix(b"");
            loop {
                select! {
                    Some(response) = futures.next() => {
                        match response {
                            Ok(outgoing_kind) => {
                                let mut prefix = match &outgoing_kind {
                                    OutgoingKind::Appservice(server) => {
                                        let mut p = b"+".to_vec();
                                        p.extend_from_slice(server.as_bytes());
                                        p
                                    }
                                    OutgoingKind::Push(id) => {
                                        let mut p = b"$".to_vec();
                                        p.extend_from_slice(&id);
                                        p
                                    },
                                    OutgoingKind::Normal(server) => {
                                        let mut p = vec![];
                                        p.extend_from_slice(server.as_bytes());
                                        p
                                    },
                                };
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

                                    futures.push(
                                        Self::handle_event(
                                            outgoing_kind.clone(),
                                            new_pdus,
                                            &db,
                                        )
                                    );
                                } else {
                                    servercurrentpdus.remove(&prefix).unwrap();
                                    // servercurrentpdus with the prefix should be empty now
                                }
                            }
                            Err((outgoing_kind, e)) => {
                                info!("Couldn't send transaction to {}\n{}", outgoing_kind, e);
                                let mut prefix = match &outgoing_kind {
                                    OutgoingKind::Appservice(serv) => {
                                        let mut p = b"+".to_vec();
                                        p.extend_from_slice(serv.as_bytes());
                                        p
                                    },
                                    OutgoingKind::Push(id) => {
                                        let mut p = b"$".to_vec();
                                        p.extend_from_slice(&id);
                                        p
                                    },
                                    OutgoingKind::Normal(serv) => {
                                        let mut p = vec![];
                                        p.extend_from_slice(serv.as_bytes());
                                        p
                                    },
                                };

                                prefix.push(0xff);

                                last_failed_try.insert(outgoing_kind.clone(), match last_failed_try.get(&outgoing_kind) {
                                    Some(last_failed) => {
                                        (last_failed.0+1, Instant::now())
                                    },
                                    None => {
                                        (1, Instant::now())
                                    }
                                });
                                servercurrentpdus.remove(&prefix).unwrap();
                            }
                        };
                    },
                    Some(event) = &mut subscriber => {
                        if let sled::Event::Insert { key, .. } = event {
                            let servernamepduid = key.clone();
                            let mut parts = servernamepduid.splitn(2, |&b| b == 0xff);

                            let exponential_backoff = |(tries, instant): &(u32, Instant)| {
                                // Fail if a request has failed recently (exponential backoff)
                                let mut min_elapsed_duration = Duration::from_secs(60) * (*tries) * (*tries);
                                if min_elapsed_duration > Duration::from_secs(60*60*24) {
                                    min_elapsed_duration = Duration::from_secs(60*60*24);
                                }

                                instant.elapsed() < min_elapsed_duration
                            };
                            if let Some((outgoing_kind, pdu_id)) = utils::string_from_bytes(
                                    parts
                                        .next()
                                        .expect("splitn will always return 1 or more elements"),
                                )
                                .map_err(|_| Error::bad_database("[Utf8] ServerName in servernamepduid bytes are invalid."))
                                .and_then(|ident_str| {
                                    // Appservices start with a plus
                                    Ok(if ident_str.starts_with('+') {
                                        OutgoingKind::Appservice(
                                            Box::<ServerName>::try_from(&ident_str[1..])
                                                .map_err(|_| Error::bad_database("ServerName in servernamepduid is invalid."))?
                                        )
                                    } else if ident_str.starts_with('$') {
                                        OutgoingKind::Push(ident_str[1..].as_bytes().to_vec())
                                    } else {
                                        OutgoingKind::Normal(
                                            Box::<ServerName>::try_from(ident_str)
                                                .map_err(|_| Error::bad_database("ServerName in servernamepduid is invalid."))?
                                        )
                                    })
                                })
                                .and_then(|outgoing_kind| parts
                                    .next()
                                    .ok_or_else(|| Error::bad_database("Invalid servernamepduid in db."))
                                    .map(|pdu_id| (outgoing_kind, pdu_id))
                                )
                                .ok()
                                .filter(|(outgoing_kind, _)| {
                                    if last_failed_try.get(outgoing_kind).map_or(false, exponential_backoff) {
                                        return false;
                                    }

                                    let mut prefix = match outgoing_kind {
                                        OutgoingKind::Appservice(serv) => {
                                            let mut p = b"+".to_vec();
                                            p.extend_from_slice(serv.as_bytes());
                                            p
                                    },
                                        OutgoingKind::Push(id) => {
                                            let mut p = b"$".to_vec();
                                            p.extend_from_slice(&id);
                                            p
                                        },
                                        OutgoingKind::Normal(serv) => {
                                            let mut p = vec![];
                                            p.extend_from_slice(serv.as_bytes());
                                            p
                                        },
                                    };
                                    prefix.push(0xff);

                                    servercurrentpdus
                                        .compare_and_swap(prefix, Option::<&[u8]>::None, Some(&[])) // Try to reserve
                                        == Ok(Ok(()))
                                })
                            {
                                servercurrentpdus.insert(&key, &[]).unwrap();
                                servernamepduids.remove(&key).unwrap();

                                futures.push(
                                    Self::handle_event(
                                        outgoing_kind,
                                        vec![pdu_id.into()],
                                        &db,
                                    )
                                );
                            }
                        }
                    }
                }
            }
        });
    }

    pub fn send_push_pdu(&self, pdu_id: &[u8]) -> Result<()> {
        // Make sure we don't cause utf8 errors when parsing to a String...
        let pduid = String::from_utf8_lossy(pdu_id).as_bytes().to_vec();

        // these are valid ServerName chars
        // (byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
        let mut key = b"$".to_vec();
        // keep each pdu push unique
        key.extend_from_slice(pduid.as_slice());
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    pub fn send_pdu(&self, server: &ServerName, pdu_id: &[u8]) -> Result<()> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    pub fn send_pdu_appservice(&self, appservice_id: &str, pdu_id: &[u8]) -> Result<()> {
        let mut key = b"+".to_vec();
        key.extend_from_slice(appservice_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    // TODO this is the whole DB but is it better to clone smaller parts than the whole thing??
    async fn handle_event(
        kind: OutgoingKind,
        pdu_ids: Vec<IVec>,
        db: &Database,
    ) -> std::result::Result<OutgoingKind, (OutgoingKind, Error)> {
        match kind {
            OutgoingKind::Appservice(server) => {
                let pdu_jsons = pdu_ids
                    .iter()
                    .map(|pdu_id| {
                        Ok::<_, (Box<ServerName>, Error)>(
                            db.rooms
                                .get_pdu_from_id(pdu_id)
                                .map_err(|e| (server.clone(), e))?
                                .ok_or_else(|| {
                                    (
                                        server.clone(),
                                        Error::bad_database(
                                            "[Appservice] Event in servernamepduids not found in ",
                                        ),
                                    )
                                })?
                                .to_any_event(),
                        )
                    })
                    .filter_map(|r| r.ok())
                    .collect::<Vec<_>>();
                appservice_server::send_request(
                    &db.globals,
                    db.appservice
                        .get_registration(server.as_str())
                        .unwrap()
                        .unwrap(), // TODO: handle error
                    appservice::event::push_events::v1::Request {
                        events: &pdu_jsons,
                        txn_id: &utils::random_string(16),
                    },
                )
                .await
                .map(|_response| OutgoingKind::Appservice(server.clone()))
                .map_err(|e| (OutgoingKind::Appservice(server.clone()), e))
            }
            OutgoingKind::Push(id) => {
                let pdus = pdu_ids
                    .iter()
                    .map(|pdu_id| {
                        Ok::<_, (Vec<u8>, Error)>(
                            db.rooms
                                .get_pdu_from_id(pdu_id)
                                .map_err(|e| (id.clone(), e))?
                                .ok_or_else(|| {
                                    (
                                        id.clone(),
                                        Error::bad_database(
                                            "[Push] Event in servernamepduids not found in db.",
                                        ),
                                    )
                                })?,
                        )
                    })
                    .filter_map(|r| r.ok())
                    .collect::<Vec<_>>();

                for pdu in &pdus {
                    // Redacted events are not notification targets (we don't send push for them)
                    if pdu.unsigned.get("redacted_because").is_some() {
                        continue;
                    }

                    // Skip events that came from the admin room
                    if db
                        .rooms
                        .room_aliases(&pdu.room_id)
                        .any(|alias| match alias {
                            Ok(a) => a.as_str().starts_with("#admins:"),
                            _ => false,
                        })
                        || pdu.sender.as_str().starts_with("@conduit:")
                    {
                        continue;
                    }

                    for user in db.rooms.room_members(&pdu.room_id) {
                        let user = user.map_err(|e| (OutgoingKind::Push(id.clone()), e))?;

                        // Don't notify the user of their own events
                        if user == pdu.sender {
                            continue;
                        }

                        let pushers = db
                            .pusher
                            .get_pusher(&user)
                            .map_err(|e| (OutgoingKind::Push(id.clone()), e))?;

                        let rules_for_user = db
                            .account_data
                            .get::<push_rules::PushRulesEvent>(None, &user, EventType::PushRules)
                            .map_err(|e| (OutgoingKind::Push(id.clone()), e))?
                            .map(|ev| ev.content.global)
                            .unwrap_or_else(|| crate::push_rules::default_pushrules(&user));

                        let unread: UInt = if let Some(last_read) = db
                            .rooms
                            .edus
                            .private_read_get(&pdu.room_id, &user)
                            .map_err(|e| (OutgoingKind::Push(id.clone()), e))?
                        {
                            (db.rooms
                                .pdus_since(&user, &pdu.room_id, last_read)
                                .map_err(|e| (OutgoingKind::Push(id.clone()), e))?
                                .filter_map(|pdu| pdu.ok()) // Filter out buggy events
                                .filter(|(_, pdu)| {
                                    matches!(
                                        pdu.kind.clone(),
                                        EventType::RoomMessage | EventType::RoomEncrypted
                                    )
                                })
                                .count() as u32)
                                .into()
                        } else {
                            // Just return zero unread messages
                            uint!(0)
                        };

                        crate::database::pusher::send_push_notice(
                            &user,
                            unread,
                            &pushers,
                            rules_for_user,
                            pdu,
                            db,
                        )
                        .await
                        .map_err(|e| (OutgoingKind::Push(id.clone()), e))?;
                    }
                }

                Ok(OutgoingKind::Push(id))
            }
            OutgoingKind::Normal(server) => {
                let pdu_jsons = pdu_ids
                    .iter()
                    .map(|pdu_id| {
                        Ok::<_, (OutgoingKind, Error)>(
                            // TODO: check room version and remove event_id if needed
                            serde_json::from_str(
                                PduEvent::convert_to_outgoing_federation_event(
                                    db.rooms
                                        .get_pdu_json_from_id(pdu_id)
                                        .map_err(|e| (OutgoingKind::Normal(server.clone()), e))?
                                        .ok_or_else(|| {
                                            (
                                                OutgoingKind::Normal(server.clone()),
                                                Error::bad_database(
                                                    "[Normal] Event in servernamepduids not found in db.",
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
                    &db.globals,
                    &*server,
                    send_transaction_message::v1::Request {
                        origin: db.globals.server_name(),
                        pdus: &pdu_jsons,
                        edus: &[],
                        origin_server_ts: SystemTime::now(),
                        transaction_id: &utils::random_string(16),
                    },
                )
                .await
                .map(|_response| OutgoingKind::Normal(server.clone()))
                .map_err(|e| (OutgoingKind::Normal(server.clone()), e))
            }
        }
    }

    fn parse_servercurrentpdus(key: IVec) -> Result<(OutgoingKind, IVec)> {
        let mut parts = key.splitn(2, |&b| b == 0xff);
        let server = parts.next().expect("splitn always returns one element");
        let pdu = parts
            .next()
            .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;

        let server = utils::string_from_bytes(&server).map_err(|_| {
            Error::bad_database("Invalid server bytes in server_currenttransaction")
        })?;

        // Appservices start with a plus
        Ok::<_, Error>(if server.starts_with('+') {
            (
                OutgoingKind::Appservice(Box::<ServerName>::try_from(server).map_err(|_| {
                    Error::bad_database("Invalid server string in server_currenttransaction")
                })?),
                IVec::from(pdu),
            )
        } else if server.starts_with('$') {
            (
                OutgoingKind::Push(server.as_bytes().to_vec()),
                IVec::from(pdu),
            )
        } else {
            (
                OutgoingKind::Normal(Box::<ServerName>::try_from(server).map_err(|_| {
                    Error::bad_database("Invalid server string in server_currenttransaction")
                })?),
                IVec::from(pdu),
            )
        })
    }

    pub async fn send_federation_request<T: OutgoingRequest>(
        &self,
        globals: &crate::database::globals::Globals,
        destination: &ServerName,
        request: T,
    ) -> Result<T::IncomingResponse>
    where
        T: Debug,
    {
        let permit = self.maximum_requests.acquire().await;
        let response = server_server::send_request(globals, destination, request).await;
        drop(permit);

        response
    }

    pub async fn send_appservice_request<T: OutgoingRequest>(
        &self,
        globals: &crate::database::globals::Globals,
        registration: serde_yaml::Value,
        request: T,
    ) -> Result<T::IncomingResponse>
    where
        T: Debug,
    {
        let permit = self.maximum_requests.acquire().await;
        let response = appservice_server::send_request(globals, registration, request).await;
        drop(permit);

        response
    }
}
