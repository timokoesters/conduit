use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    fmt::Debug,
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use crate::{
    appservice_server, database::pusher, server_server, utils, Database, Error, PduEvent, Result,
};
use federation::transactions::send_transaction_message;
use log::{error, warn};
use ring::digest;
use rocket::futures::stream::{FuturesUnordered, StreamExt};
use ruma::{
    api::{appservice, federation, OutgoingRequest},
    events::{push_rules, EventType},
    push, ServerName, UInt, UserId,
};
use sled::IVec;
use tokio::{select, sync::Semaphore};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutgoingKind {
    Appservice(Box<ServerName>),
    Push(Vec<u8>, Vec<u8>), // user and pushkey
    Normal(Box<ServerName>),
}

#[derive(Clone)]
pub struct Sending {
    /// The state for a given state hash.
    pub(super) servernamepduids: sled::Tree, // ServernamePduId = (+ / $)SenderKey / ServerName / UserId + PduId
    pub(super) servercurrentpdus: sled::Tree, // ServerCurrentPdus = (+ / $)ServerName / UserId + PduId
    pub(super) maximum_requests: Arc<Semaphore>,
}

enum TransactionStatus {
    Running,
    Failed(u32, Instant), // number of times failed, time of last failure
    Retrying(u32),        // number of times failed
}

impl Sending {
    pub fn start_handler(&self, db: &Database) {
        let servernamepduids = self.servernamepduids.clone();
        let servercurrentpdus = self.servercurrentpdus.clone();

        let db = db.clone();

        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();

            // Retry requests we could not finish yet
            let mut subscriber = servernamepduids.watch_prefix(b"");
            let mut current_transaction_status = HashMap::<Vec<u8>, TransactionStatus>::new();

            let mut initial_transactions = HashMap::<OutgoingKind, Vec<Vec<u8>>>::new();
            for (key, outgoing_kind, pdu) in servercurrentpdus
                .iter()
                .filter_map(|r| r.ok())
                .filter_map(|(key, _)| {
                    Self::parse_servercurrentpdus(&key)
                        .ok()
                        .map(|(k, p)| (key, k, p.to_vec()))
                })
            {
                let entry = initial_transactions
                    .entry(outgoing_kind.clone())
                    .or_insert_with(Vec::new);

                if entry.len() > 30 {
                    warn!(
                        "Dropping some current pdu: {:?} {:?} {:?}",
                        key, outgoing_kind, pdu
                    );
                    servercurrentpdus.remove(key).unwrap();
                    continue;
                }

                entry.push(pdu);
            }

            for (outgoing_kind, pdus) in initial_transactions {
                let mut prefix = match &outgoing_kind {
                    OutgoingKind::Appservice(server) => {
                        let mut p = b"+".to_vec();
                        p.extend_from_slice(server.as_bytes());
                        p
                    }
                    OutgoingKind::Push(user, pushkey) => {
                        let mut p = b"$".to_vec();
                        p.extend_from_slice(&user);
                        p.push(0xff);
                        p.extend_from_slice(&pushkey);
                        p
                    }
                    OutgoingKind::Normal(server) => {
                        let mut p = Vec::new();
                        p.extend_from_slice(server.as_bytes());
                        p
                    }
                };
                prefix.push(0xff);
                current_transaction_status.insert(prefix, TransactionStatus::Running);
                futures.push(Self::handle_event(outgoing_kind.clone(), dbg!(pdus), &db));
            }

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
                                    OutgoingKind::Push(user, pushkey) => {
                                        let mut p = b"$".to_vec();
                                        p.extend_from_slice(&user);
                                        p.push(0xff);
                                        p.extend_from_slice(&pushkey);
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
                                    servercurrentpdus.remove(key).unwrap();
                                }

                                // Find events that have been added since starting the last request
                                let new_pdus = servernamepduids
                                    .scan_prefix(&prefix)
                                    .keys()
                                    .filter_map(|r| r.ok())
                                    .map(|k| {
                                        k[prefix.len()..].to_vec()
                                    })
                                    .take(30)
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
                                            dbg!(new_pdus),
                                            &db,
                                        )
                                    );
                                } else {
                                    current_transaction_status.remove(&prefix);
                                }
                            }
                            Err((outgoing_kind, _)) => {
                                let mut prefix = match &outgoing_kind {
                                    OutgoingKind::Appservice(serv) => {
                                        let mut p = b"+".to_vec();
                                        p.extend_from_slice(serv.as_bytes());
                                        p
                                    },
                                    OutgoingKind::Push(user, pushkey) => {
                                        let mut p = b"$".to_vec();
                                        p.extend_from_slice(&user);
                                        p.push(0xff);
                                        p.extend_from_slice(&pushkey);
                                        p
                                    },
                                    OutgoingKind::Normal(serv) => {
                                        let mut p = vec![];
                                        p.extend_from_slice(serv.as_bytes());
                                        p
                                    },
                                };

                                prefix.push(0xff);

                                current_transaction_status.entry(prefix).and_modify(|e| *e = match e {
                                    TransactionStatus::Running => TransactionStatus::Failed(1, Instant::now()),
                                    TransactionStatus::Retrying(n) => TransactionStatus::Failed(*n+1, Instant::now()),
                                    TransactionStatus::Failed(_, _) => {
                                        error!("Request that was not even running failed?!");
                                        return
                                    },
                                });
                            }
                        };
                    },
                    Some(event) = &mut subscriber => {
                        if let sled::Event::Insert { key, .. } = event {
                        // New sled version:
                        //for (_tree, key, value_opt) in &event {
                        //    if value_opt.is_none() {
                        //        continue;
                        //    }

                            let servernamepduid = key.clone();

                            let mut retry = false;

                            if let Some((outgoing_kind, prefix, pdu_id)) = Self::parse_servercurrentpdus(&servernamepduid)
                                .ok()
                                .map(|(outgoing_kind, pdu_id)| {
                                    let mut prefix = match &outgoing_kind {
                                        OutgoingKind::Appservice(serv) => {
                                            let mut p = b"+".to_vec();
                                            p.extend_from_slice(serv.as_bytes());
                                            p
                                    },
                                        OutgoingKind::Push(user, pushkey) => {
                                            let mut p = b"$".to_vec();
                                            p.extend_from_slice(&user);
                                            p.push(0xff);
                                            p.extend_from_slice(&pushkey);
                                            p
                                        },
                                        OutgoingKind::Normal(serv) => {
                                            let mut p = vec![];
                                            p.extend_from_slice(serv.as_bytes());
                                            p
                                        },
                                    };
                                    prefix.push(0xff);

                                    (outgoing_kind, prefix, pdu_id)
                                })
                                .filter(|(_, prefix, _)| {
                                    let entry = current_transaction_status.entry(prefix.clone());
                                    let mut allow = true;

                                    entry.and_modify(|e| match e {
                                        TransactionStatus::Running | TransactionStatus::Retrying(_) => {
                                            allow = false; // already running
                                        },
                                        TransactionStatus::Failed(tries, time) => {
                                            // Fail if a request has failed recently (exponential backoff)
                                            let mut min_elapsed_duration = Duration::from_secs(30) * (*tries) * (*tries);
                                            if min_elapsed_duration > Duration::from_secs(60*60*24) {
                                                min_elapsed_duration = Duration::from_secs(60*60*24);
                                            }

                                            if time.elapsed() < min_elapsed_duration {
                                                allow = false;
                                                warn!("exponential backoff");
                                            } else {
                                                retry = true;
                                                *e = TransactionStatus::Retrying(*tries);
                                            }
                                        }
                                    }).or_insert(TransactionStatus::Running);

                                    allow
                                })
                            {
                                let mut pdus = Vec::new();

                                if retry {
                                    // We retry the previous transaction
                                    for pdu in servercurrentpdus
                                        .scan_prefix(&prefix)
                                        .filter_map(|r| r.ok())
                                        .filter_map(|(key, _)| {
                                            Self::parse_servercurrentpdus(&key)
                                                .ok()
                                                .map(|(_, p)| p.to_vec())
                                        })
                                    {
                                        pdus.push(pdu);
                                    }
                                    warn!("retrying previous transaction");
                                } else {
                                    servercurrentpdus.insert(&key, &[]).unwrap();
                                    servernamepduids.remove(&key).unwrap();
                                    pdus.push(pdu_id.to_vec());
                                    warn!("sending new transaction");
                                }
                                futures.push(
                                    Self::handle_event(
                                        outgoing_kind,
                                        dbg!(pdus),
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

    #[tracing::instrument(skip(self))]
    pub fn send_push_pdu(&self, pdu_id: &[u8], senderkey: IVec) -> Result<()> {
        let mut key = b"$".to_vec();
        key.extend_from_slice(&senderkey);
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn send_pdu(&self, server: &ServerName, pdu_id: &[u8]) -> Result<()> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn send_pdu_appservice(&self, appservice_id: &str, pdu_id: &[u8]) -> Result<()> {
        let mut key = b"+".to_vec();
        key.extend_from_slice(appservice_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(key, b"")?;

        Ok(())
    }

    #[tracing::instrument]
    fn calculate_hash(keys: &[Vec<u8>]) -> Vec<u8> {
        // We only hash the pdu's event ids, not the whole pdu
        let bytes = keys.join(&0xff);
        let hash = digest::digest(&digest::SHA256, &bytes);
        hash.as_ref().to_owned()
    }

    #[tracing::instrument(skip(db))]
    async fn handle_event(
        kind: OutgoingKind,
        pdu_ids: Vec<Vec<u8>>,
        db: &Database,
    ) -> std::result::Result<OutgoingKind, (OutgoingKind, Error)> {
        match &kind {
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
                                            "[Appservice] Event in servernamepduids not found in db.",
                                        ),
                                    )
                                })?
                                .to_any_event(),
                        )
                    })
                    .filter_map(|r| r.ok())
                    .collect::<Vec<_>>();
                let permit = db.sending.maximum_requests.acquire().await;

                let response = appservice_server::send_request(
                    &db.globals,
                    db.appservice
                        .get_registration(server.as_str())
                        .unwrap()
                        .unwrap(), // TODO: handle error
                    appservice::event::push_events::v1::Request {
                        events: &pdu_jsons,
                        txn_id: &base64::encode_config(
                            Self::calculate_hash(&pdu_ids),
                            base64::URL_SAFE_NO_PAD,
                        ),
                    },
                )
                .await
                .map(|_response| kind.clone())
                .map_err(|e| (kind, e));

                drop(permit);

                response
            }
            OutgoingKind::Push(user, pushkey) => {
                let pdus = pdu_ids
                    .iter()
                    .map(|pdu_id| {
                        Ok::<_, (Vec<u8>, Error)>(
                            db.rooms
                                .get_pdu_from_id(pdu_id)
                                .map_err(|e| (pushkey.clone(), e))?
                                .ok_or_else(|| {
                                    (
                                        pushkey.clone(),
                                        Error::bad_database(
                                            "[Push] Event in servernamepduids not found in db.",
                                        ),
                                    )
                                })?,
                        )
                    })
                    .filter_map(|r| r.ok())
                    .collect::<Vec<_>>();

                for pdu in pdus {
                    // Redacted events are not notification targets (we don't send push for them)
                    if pdu.unsigned.get("redacted_because").is_some() {
                        continue;
                    }

                    let userid =
                        UserId::try_from(utils::string_from_bytes(user).map_err(|_| {
                            (
                                OutgoingKind::Push(user.clone(), pushkey.clone()),
                                Error::bad_database("Invalid push user string in db."),
                            )
                        })?)
                        .map_err(|_| {
                            (
                                OutgoingKind::Push(user.clone(), pushkey.clone()),
                                Error::bad_database("Invalid push user id in db."),
                            )
                        })?;

                    let mut senderkey = user.clone();
                    senderkey.push(0xff);
                    senderkey.extend_from_slice(pushkey);

                    let pusher = match db
                        .pusher
                        .get_pusher(&senderkey)
                        .map_err(|e| (OutgoingKind::Push(user.clone(), pushkey.clone()), e))?
                    {
                        Some(pusher) => pusher,
                        None => continue,
                    };

                    let rules_for_user = db
                        .account_data
                        .get::<push_rules::PushRulesEvent>(None, &userid, EventType::PushRules)
                        .unwrap_or_default()
                        .map(|ev| ev.content.global)
                        .unwrap_or_else(|| push::Ruleset::server_default(&userid));

                    let unread: UInt = db
                        .rooms
                        .notification_count(&userid, &pdu.room_id)
                        .map_err(|e| (kind.clone(), e))?
                        .try_into()
                        .expect("notifiation count can't go that high");

                    let permit = db.sending.maximum_requests.acquire().await;

                    let _response = pusher::send_push_notice(
                        &userid,
                        unread,
                        &pusher,
                        rules_for_user,
                        &pdu,
                        db,
                    )
                    .await
                    .map(|_response| kind.clone())
                    .map_err(|e| (kind.clone(), e));

                    drop(permit);
                }
                Ok(OutgoingKind::Push(user.clone(), pushkey.clone()))
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

                let permit = db.sending.maximum_requests.acquire().await;

                let response = server_server::send_request(
                    &db.globals,
                    &*server,
                    send_transaction_message::v1::Request {
                        origin: db.globals.server_name(),
                        pdus: &pdu_jsons,
                        edus: &[],
                        origin_server_ts: SystemTime::now(),
                        transaction_id: &base64::encode_config(
                            Self::calculate_hash(&pdu_ids),
                            base64::URL_SAFE_NO_PAD,
                        ),
                    },
                )
                .await
                .map(|response| {
                    for pdu in response.pdus {
                        if pdu.1.is_err() {
                            warn!("Failed to send to {}: {:?}", server, pdu);
                        }
                    }
                    kind.clone()
                })
                .map_err(|e| (kind, e));

                drop(permit);

                response
            }
        }
    }

    fn parse_servercurrentpdus(key: &IVec) -> Result<(OutgoingKind, IVec)> {
        // Appservices start with a plus
        Ok::<_, Error>(if key.starts_with(b"+") {
            let mut parts = key[1..].splitn(2, |&b| b == 0xff);

            let server = parts.next().expect("splitn always returns one element");
            let pdu = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let server = utils::string_from_bytes(&server).map_err(|_| {
                Error::bad_database("Invalid server bytes in server_currenttransaction")
            })?;

            (
                OutgoingKind::Appservice(Box::<ServerName>::try_from(server).map_err(|_| {
                    Error::bad_database("Invalid server string in server_currenttransaction")
                })?),
                IVec::from(pdu),
            )
        } else if key.starts_with(b"$") {
            let mut parts = key[1..].splitn(3, |&b| b == 0xff);

            let user = parts.next().expect("splitn always returns one element");
            let pushkey = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let pdu = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            (
                OutgoingKind::Push(user.to_vec(), pushkey.to_vec()),
                IVec::from(pdu),
            )
        } else {
            let mut parts = key.splitn(2, |&b| b == 0xff);

            let server = parts.next().expect("splitn always returns one element");
            let pdu = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let server = utils::string_from_bytes(&server).map_err(|_| {
                Error::bad_database("Invalid server bytes in server_currenttransaction")
            })?;

            (
                OutgoingKind::Normal(Box::<ServerName>::try_from(server).map_err(|_| {
                    Error::bad_database("Invalid server string in server_currenttransaction")
                })?),
                IVec::from(pdu),
            )
        })
    }

    #[tracing::instrument(skip(self, globals))]
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

    #[tracing::instrument(skip(self, globals))]
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
