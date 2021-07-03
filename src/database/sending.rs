use std::{
    collections::{BTreeMap, HashMap},
    convert::{TryFrom, TryInto},
    fmt::Debug,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    appservice_server, database::pusher, server_server, utils, Database, Error, PduEvent, Result,
};
use federation::transactions::send_transaction_message;
use log::{error, warn};
use ring::digest;
use rocket::futures::{
    channel::mpsc,
    stream::{FuturesUnordered, StreamExt},
};
use ruma::{
    api::{
        appservice,
        federation::{
            self,
            transactions::edu::{Edu, ReceiptContent, ReceiptData, ReceiptMap},
        },
        OutgoingRequest,
    },
    events::{push_rules, AnySyncEphemeralRoomEvent, EventType},
    push,
    receipt::ReceiptType,
    MilliSecondsSinceUnixEpoch, ServerName, UInt, UserId,
};
use tokio::{select, sync::{Semaphore, RwLock}};

use super::abstraction::Tree;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutgoingKind {
    Appservice(Box<ServerName>),
    Push(Vec<u8>, Vec<u8>), // user and pushkey
    Normal(Box<ServerName>),
}

impl OutgoingKind {
    pub fn get_prefix(&self) -> Vec<u8> {
        let mut prefix = match self {
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

        prefix
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SendingEventType {
    Pdu(Vec<u8>),
    Edu(Vec<u8>),
}

pub struct Sending {
    /// The state for a given state hash.
    pub(super) servername_educount: Arc<dyn Tree>, // EduCount: Count of last EDU sync
    pub(super) servernamepduids: Arc<dyn Tree>, // ServernamePduId = (+ / $)SenderKey / ServerName / UserId + PduId
    pub(super) servercurrentevents: Arc<dyn Tree>, // ServerCurrentEvents = (+ / $)ServerName / UserId + PduId / (*)EduEvent
    pub(super) maximum_requests: Arc<Semaphore>,
    pub sender: mpsc::UnboundedSender<Vec<u8>>,
}

enum TransactionStatus {
    Running,
    Failed(u32, Instant), // number of times failed, time of last failure
    Retrying(u32),        // number of times failed
}

impl Sending {
    pub fn start_handler(&self, db: Arc<RwLock<Database>>, mut receiver: mpsc::UnboundedReceiver<Vec<u8>>) {
        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();

            let mut current_transaction_status = HashMap::<Vec<u8>, TransactionStatus>::new();

            // Retry requests we could not finish yet
            let mut initial_transactions = HashMap::<OutgoingKind, Vec<SendingEventType>>::new();

            let guard = db.read().await;

            for (key, outgoing_kind, event) in
                guard.sending
                    .servercurrentevents
                    .iter()
                    .filter_map(|(key, _)| {
                        Self::parse_servercurrentevent(&key)
                            .ok()
                            .map(|(k, e)| (key, k, e))
                    })
            {
                let entry = initial_transactions
                    .entry(outgoing_kind.clone())
                    .or_insert_with(Vec::new);

                if entry.len() > 30 {
                    warn!(
                        "Dropping some current events: {:?} {:?} {:?}",
                        key, outgoing_kind, event
                    );
                    guard.sending.servercurrentevents.remove(&key).unwrap();
                    continue;
                }

                entry.push(event);
            }

            drop(guard);

            for (outgoing_kind, events) in initial_transactions {
                current_transaction_status
                    .insert(outgoing_kind.get_prefix(), TransactionStatus::Running);
                futures.push(Self::handle_events(outgoing_kind.clone(), events, Arc::clone(&db)));
            }

            loop {
                select! {
                    Some(response) = futures.next() => {
                        match response {
                            Ok(outgoing_kind) => {
                                let guard = db.read().await;

                                let prefix = outgoing_kind.get_prefix();
                                for (key, _) in guard.sending.servercurrentevents
                                    .scan_prefix(prefix.clone())
                                {
                                    guard.sending.servercurrentevents.remove(&key).unwrap();
                                }

                                // Find events that have been added since starting the last request
                                let new_events = guard.sending.servernamepduids
                                    .scan_prefix(prefix.clone())
                                    .map(|(k, _)| {
                                        SendingEventType::Pdu(k[prefix.len()..].to_vec())
                                    })
                                    .take(30)
                                    .collect::<Vec<_>>();

                                // TODO: find edus

                                if !new_events.is_empty() {
                                    // Insert pdus we found
                                    for event in &new_events {
                                        let mut current_key = prefix.clone();
                                        match event {
                                            SendingEventType::Pdu(b) |
                                            SendingEventType::Edu(b) => {
                                                current_key.extend_from_slice(&b);
                                                guard.sending.servercurrentevents.insert(&current_key, &[]).unwrap();
                                                guard.sending.servernamepduids.remove(&current_key).unwrap();
                                             }
                                        }
                                    }

                                    drop(guard);

                                    futures.push(
                                        Self::handle_events(
                                            outgoing_kind.clone(),
                                            new_events,
                                            Arc::clone(&db),
                                        )
                                    );
                                } else {
                                    current_transaction_status.remove(&prefix);
                                }
                            }
                            Err((outgoing_kind, _)) => {
                                current_transaction_status.entry(outgoing_kind.get_prefix()).and_modify(|e| *e = match e {
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
                    Some(key) = receiver.next() => {
                        if let Ok((outgoing_kind, event)) = Self::parse_servercurrentevent(&key) {
                            let guard = db.read().await;

                            if let Ok(Some(events)) = Self::select_events(
                                &outgoing_kind,
                                vec![(event, key)],
                                &mut current_transaction_status,
                                &guard
                            ) {
                                futures.push(Self::handle_events(outgoing_kind, events, Arc::clone(&db)));
                            }
                        }
                    }
                }
            }
        });
    }

    fn select_events(
        outgoing_kind: &OutgoingKind,
        new_events: Vec<(SendingEventType, Vec<u8>)>, // Events we want to send: event and full key
        current_transaction_status: &mut HashMap<Vec<u8>, TransactionStatus>,
        db: &Database,
    ) -> Result<Option<Vec<SendingEventType>>> {
        let mut retry = false;
        let mut allow = true;

        let prefix = outgoing_kind.get_prefix();
        let entry = current_transaction_status.entry(prefix.clone());

        entry
            .and_modify(|e| match e {
                TransactionStatus::Running | TransactionStatus::Retrying(_) => {
                    allow = false; // already running
                }
                TransactionStatus::Failed(tries, time) => {
                    // Fail if a request has failed recently (exponential backoff)
                    let mut min_elapsed_duration = Duration::from_secs(30) * (*tries) * (*tries);
                    if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
                        min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
                    }

                    if time.elapsed() < min_elapsed_duration {
                        allow = false;
                    } else {
                        retry = true;
                        *e = TransactionStatus::Retrying(*tries);
                    }
                }
            })
            .or_insert(TransactionStatus::Running);

        if !allow {
            return Ok(None);
        }

        let mut events = Vec::new();

        if retry {
            // We retry the previous transaction
            for (key, _) in db.sending.servercurrentevents.scan_prefix(prefix) {
                if let Ok((_, e)) = Self::parse_servercurrentevent(&key) {
                    events.push(e);
                }
            }
        } else {
            for (e, full_key) in new_events {
                db.sending.servercurrentevents.insert(&full_key, &[])?;

                // If it was a PDU we have to unqueue it
                // TODO: don't try to unqueue EDUs
                db.sending.servernamepduids.remove(&full_key)?;

                events.push(e);
            }

            if let OutgoingKind::Normal(server_name) = outgoing_kind {
                if let Ok((select_edus, last_count)) = Self::select_edus(db, server_name) {
                    events.extend_from_slice(&select_edus);
                    db.sending
                        .servername_educount
                        .insert(server_name.as_bytes(), &last_count.to_be_bytes())?;
                }
            }
        }

        Ok(Some(events))
    }

    pub fn select_edus(db: &Database, server: &ServerName) -> Result<(Vec<SendingEventType>, u64)> {
        // u64: count of last edu
        let since = db
            .sending
            .servername_educount
            .get(server.as_bytes())?
            .map_or(Ok(0), |bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid u64 in servername_educount."))
            })?;
        let mut events = Vec::new();
        let mut max_edu_count = since;
        'outer: for room_id in db.rooms.server_rooms(server) {
            let room_id = room_id?;
            for r in db.rooms.edus.readreceipts_since(&room_id, since) {
                let (user_id, count, read_receipt) = r?;

                if count > max_edu_count {
                    max_edu_count = count;
                }

                if user_id.server_name() != db.globals.server_name() {
                    continue;
                }

                let event =
                    serde_json::from_str::<AnySyncEphemeralRoomEvent>(&read_receipt.json().get())
                        .map_err(|_| Error::bad_database("Invalid edu event in read_receipts."))?;
                let federation_event = match event {
                    AnySyncEphemeralRoomEvent::Receipt(r) => {
                        let mut read = BTreeMap::new();

                        let (event_id, mut receipt) = r
                            .content
                            .0
                            .into_iter()
                            .next()
                            .expect("we only use one event per read receipt");
                        let receipt = receipt
                            .remove(&ReceiptType::Read)
                            .expect("our read receipts always set this")
                            .remove(&user_id)
                            .expect("our read receipts always have the user here");

                        read.insert(
                            user_id,
                            ReceiptData {
                                data: receipt.clone(),
                                event_ids: vec![event_id.clone()],
                            },
                        );

                        let receipt_map = ReceiptMap { read };

                        let mut receipts = BTreeMap::new();
                        receipts.insert(room_id.clone(), receipt_map);

                        Edu::Receipt(ReceiptContent { receipts })
                    }
                    _ => {
                        Error::bad_database("Invalid event type in read_receipts");
                        continue;
                    }
                };

                events.push(SendingEventType::Edu(
                    serde_json::to_vec(&federation_event).expect("json can be serialized"),
                ));

                if events.len() >= 20 {
                    break 'outer;
                }
            }
        }

        Ok((events, max_edu_count))
    }

    #[tracing::instrument(skip(self))]
    pub fn send_push_pdu(&self, pdu_id: &[u8], senderkey: Vec<u8>) -> Result<()> {
        let mut key = b"$".to_vec();
        key.extend_from_slice(&senderkey);
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(&key, b"")?;
        self.sender.unbounded_send(key).unwrap();

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn send_pdu(&self, server: &ServerName, pdu_id: &[u8]) -> Result<()> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(&key, b"")?;
        self.sender.unbounded_send(key).unwrap();

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn send_pdu_appservice(&self, appservice_id: &str, pdu_id: &[u8]) -> Result<()> {
        let mut key = b"+".to_vec();
        key.extend_from_slice(appservice_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernamepduids.insert(&key, b"")?;
        self.sender.unbounded_send(key).unwrap();

        Ok(())
    }

    #[tracing::instrument]
    fn calculate_hash(keys: &[&[u8]]) -> Vec<u8> {
        // We only hash the pdu's event ids, not the whole pdu
        let bytes = keys.join(&0xff);
        let hash = digest::digest(&digest::SHA256, &bytes);
        hash.as_ref().to_owned()
    }

    #[tracing::instrument(skip(db))]
    async fn handle_events(
        kind: OutgoingKind,
        events: Vec<SendingEventType>,
        db: Arc<RwLock<Database>>,
    ) -> std::result::Result<OutgoingKind, (OutgoingKind, Error)> {
        let db = db.read().await;

        match &kind {
            OutgoingKind::Appservice(server) => {
                let mut pdu_jsons = Vec::new();

                for event in &events {
                    match event {
                        SendingEventType::Pdu(pdu_id) => {
                            pdu_jsons.push(db.rooms
                                .get_pdu_from_id(&pdu_id)
                                .map_err(|e| (kind.clone(), e))?
                                .ok_or_else(|| {
                                    (
                                        kind.clone(),
                                        Error::bad_database(
                                            "[Appservice] Event in servernamepduids not found in db.",
                                        ),
                                    )
                                })?
                                .to_room_event())
                        }
                        SendingEventType::Edu(_) => {
                            // Appservices don't need EDUs (?)
                        }
                    }
                }

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
                            Self::calculate_hash(
                                &events
                                    .iter()
                                    .map(|e| match e {
                                        SendingEventType::Edu(b) | SendingEventType::Pdu(b) => &**b,
                                    })
                                    .collect::<Vec<_>>(),
                            ),
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
                let mut pdus = Vec::new();

                for event in &events {
                    match event {
                        SendingEventType::Pdu(pdu_id) => {
                            pdus.push(
                                db.rooms
                                    .get_pdu_from_id(&pdu_id)
                                    .map_err(|e| (kind.clone(), e))?
                                    .ok_or_else(|| {
                                        (
                                            kind.clone(),
                                            Error::bad_database(
                                                "[Push] Event in servernamepduids not found in db.",
                                            ),
                                        )
                                    })?,
                            );
                        }
                        SendingEventType::Edu(_) => {
                            // Push gateways don't need EDUs (?)
                        }
                    }
                }

                for pdu in pdus {
                    // Redacted events are not notification targets (we don't send push for them)
                    if pdu.unsigned.get("redacted_because").is_some() {
                        continue;
                    }

                    let userid =
                        UserId::try_from(utils::string_from_bytes(user).map_err(|_| {
                            (
                                kind.clone(),
                                Error::bad_database("Invalid push user string in db."),
                            )
                        })?)
                        .map_err(|_| {
                            (
                                kind.clone(),
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
                        &db,
                    )
                    .await
                    .map(|_response| kind.clone())
                    .map_err(|e| (kind.clone(), e));

                    drop(permit);
                }
                Ok(OutgoingKind::Push(user.clone(), pushkey.clone()))
            }
            OutgoingKind::Normal(server) => {
                let mut edu_jsons = Vec::new();
                let mut pdu_jsons = Vec::new();

                for event in &events {
                    match event {
                        SendingEventType::Pdu(pdu_id) => {
                            // TODO: check room version and remove event_id if needed
                            pdu_jsons.push(serde_json::from_str(
                                PduEvent::convert_to_outgoing_federation_event(
                                    db.rooms
                                        .get_pdu_json_from_id(&pdu_id)
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
                            .expect("Raw<..> is always valid"));
                        }
                        SendingEventType::Edu(edu) => {
                            edu_jsons.push(
                                serde_json::from_slice(edu).expect("Raw<..> is always valid"),
                            );
                        }
                    }
                }

                let permit = db.sending.maximum_requests.acquire().await;

                let response = server_server::send_request(
                    &db.globals,
                    &*server,
                    send_transaction_message::v1::Request {
                        origin: db.globals.server_name(),
                        pdus: &pdu_jsons,
                        edus: &edu_jsons,
                        origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
                        transaction_id: &base64::encode_config(
                            Self::calculate_hash(
                                &events
                                    .iter()
                                    .map(|e| match e {
                                        SendingEventType::Edu(b) | SendingEventType::Pdu(b) => &**b,
                                    })
                                    .collect::<Vec<_>>(),
                            ),
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

    fn parse_servercurrentevent(key: &[u8]) -> Result<(OutgoingKind, SendingEventType)> {
        // Appservices start with a plus
        Ok::<_, Error>(if key.starts_with(b"+") {
            let mut parts = key[1..].splitn(2, |&b| b == 0xff);

            let server = parts.next().expect("splitn always returns one element");
            let event = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let server = utils::string_from_bytes(&server).map_err(|_| {
                Error::bad_database("Invalid server bytes in server_currenttransaction")
            })?;

            (
                OutgoingKind::Appservice(Box::<ServerName>::try_from(server).map_err(|_| {
                    Error::bad_database("Invalid server string in server_currenttransaction")
                })?),
                if event.starts_with(b"*") {
                    SendingEventType::Edu(event[1..].to_vec())
                } else {
                    SendingEventType::Pdu(event.to_vec())
                },
            )
        } else if key.starts_with(b"$") {
            let mut parts = key[1..].splitn(3, |&b| b == 0xff);

            let user = parts.next().expect("splitn always returns one element");
            let pushkey = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let event = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            (
                OutgoingKind::Push(user.to_vec(), pushkey.to_vec()),
                if event.starts_with(b"*") {
                    SendingEventType::Edu(event[1..].to_vec())
                } else {
                    SendingEventType::Pdu(event.to_vec())
                },
            )
        } else {
            let mut parts = key.splitn(2, |&b| b == 0xff);

            let server = parts.next().expect("splitn always returns one element");
            let event = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let server = utils::string_from_bytes(&server).map_err(|_| {
                Error::bad_database("Invalid server bytes in server_currenttransaction")
            })?;

            (
                OutgoingKind::Normal(Box::<ServerName>::try_from(server).map_err(|_| {
                    Error::bad_database("Invalid server string in server_currenttransaction")
                })?),
                if event.starts_with(b"*") {
                    SendingEventType::Edu(event[1..].to_vec())
                } else {
                    SendingEventType::Pdu(event.to_vec())
                },
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
