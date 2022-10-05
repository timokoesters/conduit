use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Debug,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    api::{appservice_server, server_server},
    services,
    utils::{self, calculate_hash},
    Error, PduEvent, Result,
};
use federation::transactions::send_transaction_message;
use futures_util::{stream::FuturesUnordered, StreamExt};
use ring::digest;
use ruma::{
    api::{
        appservice,
        federation::{
            self,
            transactions::edu::{
                DeviceListUpdateContent, Edu, ReceiptContent, ReceiptData, ReceiptMap,
            },
        },
        OutgoingRequest,
    },
    device_id,
    events::{push_rules::PushRulesEvent, AnySyncEphemeralRoomEvent, GlobalAccountDataEventType},
    push,
    receipt::ReceiptType,
    uint, MilliSecondsSinceUnixEpoch, ServerName, UInt, UserId,
};
use tokio::{
    select,
    sync::{mpsc, RwLock, Semaphore},
};
use tracing::{error, warn};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutgoingKind {
    Appservice(String),
    Push(Vec<u8>, Vec<u8>), // user and pushkey
    Normal(Box<ServerName>),
}

impl OutgoingKind {
    #[tracing::instrument(skip(self))]
    pub fn get_prefix(&self) -> Vec<u8> {
        let mut prefix = match self {
            OutgoingKind::Appservice(server) => {
                let mut p = b"+".to_vec();
                p.extend_from_slice(server.as_bytes());
                p
            }
            OutgoingKind::Push(user, pushkey) => {
                let mut p = b"$".to_vec();
                p.extend_from_slice(user);
                p.push(0xff);
                p.extend_from_slice(pushkey);
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

pub struct Service {
    /// The state for a given state hash.
    pub(super) maximum_requests: Arc<Semaphore>,
    pub sender: mpsc::UnboundedSender<(Vec<u8>, Vec<u8>)>,
}

enum TransactionStatus {
    Running,
    Failed(u32, Instant), // number of times failed, time of last failure
    Retrying(u32),        // number of times failed
}

impl Service {
    pub fn start_handler(&self, mut receiver: mpsc::UnboundedReceiver<(Vec<u8>, Vec<u8>)>) {
        tokio::spawn(async move {
            let mut futures = FuturesUnordered::new();

            let mut current_transaction_status = HashMap::<Vec<u8>, TransactionStatus>::new();

            // Retry requests we could not finish yet
            let mut initial_transactions = HashMap::<OutgoingKind, Vec<SendingEventType>>::new();

            for (key, outgoing_kind, event) in services()
                .sending
                .servercurrentevent_data
                .iter()
                .filter_map(|(key, v)| {
                    Self::parse_servercurrentevent(&key, v)
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
                    services()
                        .sending
                        .servercurrentevent_data
                        .remove(&key)
                        .unwrap();
                    continue;
                }

                entry.push(event);
            }

            for (outgoing_kind, events) in initial_transactions {
                current_transaction_status
                    .insert(outgoing_kind.get_prefix(), TransactionStatus::Running);
                futures.push(Self::handle_events(outgoing_kind.clone(), events));
            }

            loop {
                select! {
                    Some(response) = futures.next() => {
                        match response {
                            Ok(outgoing_kind) => {
                                let prefix = outgoing_kind.get_prefix();
                                for (key, _) in services().sending.servercurrentevent_data
                                    .scan_prefix(prefix.clone())
                                {
                                    services().sending.servercurrentevent_data.remove(&key).unwrap();
                                }

                                // Find events that have been added since starting the last request
                                let new_events: Vec<_> = services().sending.servernameevent_data
                                    .scan_prefix(prefix.clone())
                                    .filter_map(|(k, v)| {
                                        Self::parse_servercurrentevent(&k, v).ok().map(|ev| (ev, k))
                                    })
                                    .take(30)
                                    .collect();

                                // TODO: find edus

                                if !new_events.is_empty() {
                                    // Insert pdus we found
                                    for (e, key) in &new_events {
                                        let value = if let SendingEventType::Edu(value) = &e.1 { &**value } else { &[] };
                                        services().sending.servercurrentevent_data.insert(key, value).unwrap();
                                        services().sending.servernameevent_data.remove(key).unwrap();
                                    }

                                    futures.push(
                                        Self::handle_events(
                                            outgoing_kind.clone(),
                                            new_events.into_iter().map(|(event, _)| event.1).collect(),
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
                    Some((key, value)) = receiver.recv() => {
                        if let Ok((outgoing_kind, event)) = Self::parse_servercurrentevent(&key, value) {
                            if let Ok(Some(events)) = Self::select_events(
                                &outgoing_kind,
                                vec![(event, key)],
                                &mut current_transaction_status,
                            ) {
                                futures.push(Self::handle_events(outgoing_kind, events));
                            }
                        }
                    }
                }
            }
        });
    }

    #[tracing::instrument(skip(outgoing_kind, new_events, current_transaction_status))]
    fn select_events(
        outgoing_kind: &OutgoingKind,
        new_events: Vec<(SendingEventType, Vec<u8>)>, // Events we want to send: event and full key
        current_transaction_status: &mut HashMap<Vec<u8>, TransactionStatus>,
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
            for (key, value) in services()
                .sending
                .servercurrentevent_data
                .scan_prefix(prefix)
            {
                if let Ok((_, e)) = Self::parse_servercurrentevent(&key, value) {
                    events.push(e);
                }
            }
        } else {
            for (e, full_key) in new_events {
                let value = if let SendingEventType::Edu(value) = &e {
                    &**value
                } else {
                    &[][..]
                };
                services()
                    .sending
                    .servercurrentevent_data
                    .insert(&full_key, value)?;

                // If it was a PDU we have to unqueue it
                // TODO: don't try to unqueue EDUs
                services().sending.servernameevent_data.remove(&full_key)?;

                events.push(e);
            }

            if let OutgoingKind::Normal(server_name) = outgoing_kind {
                if let Ok((select_edus, last_count)) = Self::select_edus(server_name) {
                    events.extend(select_edus.into_iter().map(SendingEventType::Edu));

                    services()
                        .sending
                        .servername_educount
                        .insert(server_name.as_bytes(), &last_count.to_be_bytes())?;
                }
            }
        }

        Ok(Some(events))
    }

    #[tracing::instrument(skip(server))]
    pub fn select_edus(server: &ServerName) -> Result<(Vec<Vec<u8>>, u64)> {
        // u64: count of last edu
        let since = services()
            .sending
            .servername_educount
            .get(server.as_bytes())?
            .map_or(Ok(0), |bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid u64 in servername_educount."))
            })?;
        let mut events = Vec::new();
        let mut max_edu_count = since;
        let mut device_list_changes = HashSet::new();

        'outer: for room_id in services().rooms.server_rooms(server) {
            let room_id = room_id?;
            // Look for device list updates in this room
            device_list_changes.extend(
                services()
                    .users
                    .keys_changed(&room_id.to_string(), since, None)
                    .filter_map(|r| r.ok())
                    .filter(|user_id| user_id.server_name() == services().globals.server_name()),
            );

            // Look for read receipts in this room
            for r in services().rooms.edus.readreceipts_since(&room_id, since) {
                let (user_id, count, read_receipt) = r?;

                if count > max_edu_count {
                    max_edu_count = count;
                }

                if user_id.server_name() != services().globals.server_name() {
                    continue;
                }

                let event: AnySyncEphemeralRoomEvent =
                    serde_json::from_str(read_receipt.json().get())
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

                events.push(serde_json::to_vec(&federation_event).expect("json can be serialized"));

                if events.len() >= 20 {
                    break 'outer;
                }
            }
        }

        for user_id in device_list_changes {
            // Empty prev id forces synapse to resync: https://github.com/matrix-org/synapse/blob/98aec1cc9da2bd6b8e34ffb282c85abf9b8b42ca/synapse/handlers/device.py#L767
            // Because synapse resyncs, we can just insert dummy data
            let edu = Edu::DeviceListUpdate(DeviceListUpdateContent {
                user_id,
                device_id: device_id!("dummy").to_owned(),
                device_display_name: Some("Dummy".to_owned()),
                stream_id: uint!(1),
                prev_id: Vec::new(),
                deleted: None,
                keys: None,
            });

            events.push(serde_json::to_vec(&edu).expect("json can be serialized"));
        }

        Ok((events, max_edu_count))
    }

    #[tracing::instrument(skip(self, pdu_id, senderkey))]
    pub fn send_push_pdu(&self, pdu_id: &[u8], senderkey: Vec<u8>) -> Result<()> {
        let mut key = b"$".to_vec();
        key.extend_from_slice(&senderkey);
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernameevent_data.insert(&key, &[])?;
        self.sender.send((key, vec![])).unwrap();

        Ok(())
    }

    #[tracing::instrument(skip(self, servers, pdu_id))]
    pub fn send_pdu<I: Iterator<Item = Box<ServerName>>>(
        &self,
        servers: I,
        pdu_id: &[u8],
    ) -> Result<()> {
        let mut batch = servers.map(|server| {
            let mut key = server.as_bytes().to_vec();
            key.push(0xff);
            key.extend_from_slice(pdu_id);

            self.sender.send((key.clone(), vec![])).unwrap();

            (key, Vec::new())
        });

        self.servernameevent_data.insert_batch(&mut batch)?;

        Ok(())
    }

    #[tracing::instrument(skip(self, server, serialized))]
    pub fn send_reliable_edu(
        &self,
        server: &ServerName,
        serialized: Vec<u8>,
        id: u64,
    ) -> Result<()> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&id.to_be_bytes());
        self.servernameevent_data.insert(&key, &serialized)?;
        self.sender.send((key, serialized)).unwrap();

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn send_pdu_appservice(&self, appservice_id: &str, pdu_id: &[u8]) -> Result<()> {
        let mut key = b"+".to_vec();
        key.extend_from_slice(appservice_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(pdu_id);
        self.servernameevent_data.insert(&key, &[])?;
        self.sender.send((key, vec![])).unwrap();

        Ok(())
    }

    /// Cleanup event data
    /// Used for instance after we remove an appservice registration
    ///
    #[tracing::instrument(skip(self))]
    pub fn cleanup_events(&self, key_id: &str) -> Result<()> {
        let mut prefix = b"+".to_vec();
        prefix.extend_from_slice(key_id.as_bytes());
        prefix.push(0xff);

        for (key, _) in self.servercurrentevent_data.scan_prefix(prefix.clone()) {
            self.servercurrentevent_data.remove(&key).unwrap();
        }

        for (key, _) in self.servernameevent_data.scan_prefix(prefix.clone()) {
            self.servernameevent_data.remove(&key).unwrap();
        }

        Ok(())
    }

    #[tracing::instrument(skip(events, kind))]
    async fn handle_events(
        kind: OutgoingKind,
        events: Vec<SendingEventType>,
    ) -> Result<OutgoingKind, (OutgoingKind, Error)> {
        match &kind {
            OutgoingKind::Appservice(id) => {
                let mut pdu_jsons = Vec::new();

                for event in &events {
                    match event {
                        SendingEventType::Pdu(pdu_id) => {
                            pdu_jsons.push(services().rooms
                                .get_pdu_from_id(pdu_id)
                                .map_err(|e| (kind.clone(), e))?
                                .ok_or_else(|| {
                                    (
                                        kind.clone(),
                                        Error::bad_database(
                                            "[Appservice] Event in servernameevent_data not found in db.",
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

                let permit = services().sending.maximum_requests.acquire().await;

                let response = appservice_server::send_request(
                    services()
                        .appservice
                        .get_registration(&id)
                        .map_err(|e| (kind.clone(), e))?
                        .ok_or_else(|| {
                            (
                                kind.clone(),
                                Error::bad_database(
                                    "[Appservice] Could not load registration from db.",
                                ),
                            )
                        })?,
                    appservice::event::push_events::v1::Request {
                        events: &pdu_jsons,
                        txn_id: (&*base64::encode_config(
                            Self::calculate_hash(
                                &events
                                    .iter()
                                    .map(|e| match e {
                                        SendingEventType::Edu(b) | SendingEventType::Pdu(b) => &**b,
                                    })
                                    .collect::<Vec<_>>(),
                            ),
                            base64::URL_SAFE_NO_PAD,
                        ))
                            .into(),
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
                                services().rooms
                                    .get_pdu_from_id(pdu_id)
                                    .map_err(|e| (kind.clone(), e))?
                                    .ok_or_else(|| {
                                        (
                                            kind.clone(),
                                            Error::bad_database(
                                                "[Push] Event in servernamevent_datas not found in db.",
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
                    if let Some(unsigned) = &pdu.unsigned {
                        if let Ok(unsigned) =
                            serde_json::from_str::<serde_json::Value>(unsigned.get())
                        {
                            if unsigned.get("redacted_because").is_some() {
                                continue;
                            }
                        }
                    }

                    let userid = UserId::parse(utils::string_from_bytes(user).map_err(|_| {
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

                    let pusher = match services()
                        .pusher
                        .get_pusher(&senderkey)
                        .map_err(|e| (OutgoingKind::Push(user.clone(), pushkey.clone()), e))?
                    {
                        Some(pusher) => pusher,
                        None => continue,
                    };

                    let rules_for_user = services()
                        .account_data
                        .get(
                            None,
                            &userid,
                            GlobalAccountDataEventType::PushRules.to_string().into(),
                        )
                        .unwrap_or_default()
                        .map(|ev: PushRulesEvent| ev.content.global)
                        .unwrap_or_else(|| push::Ruleset::server_default(&userid));

                    let unread: UInt = services()
                        .rooms
                        .notification_count(&userid, &pdu.room_id)
                        .map_err(|e| (kind.clone(), e))?
                        .try_into()
                        .expect("notifiation count can't go that high");

                    let permit = services().sending.maximum_requests.acquire().await;

                    let _response = services()
                        .pusher
                        .send_push_notice(&userid, unread, &pusher, rules_for_user, &pdu)
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
                            let raw = PduEvent::convert_to_outgoing_federation_event(
                                services().rooms
                                    .get_pdu_json_from_id(pdu_id)
                                    .map_err(|e| (OutgoingKind::Normal(server.clone()), e))?
                                    .ok_or_else(|| {
                                        (
                                            OutgoingKind::Normal(server.clone()),
                                            Error::bad_database(
                                                "[Normal] Event in servernamevent_datas not found in db.",
                                            ),
                                        )
                                    })?,
                            );
                            pdu_jsons.push(raw);
                        }
                        SendingEventType::Edu(edu) => {
                            if let Ok(raw) = serde_json::from_slice(edu) {
                                edu_jsons.push(raw);
                            }
                        }
                    }
                }

                let permit = services().sending.maximum_requests.acquire().await;

                let response = server_server::send_request(
                    &*server,
                    send_transaction_message::v1::Request {
                        origin: services().globals.server_name(),
                        pdus: &pdu_jsons,
                        edus: &edu_jsons,
                        origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
                        transaction_id: (&*base64::encode_config(
                            calculate_hash(
                                &events
                                    .iter()
                                    .map(|e| match e {
                                        SendingEventType::Edu(b) | SendingEventType::Pdu(b) => &**b,
                                    })
                                    .collect::<Vec<_>>(),
                            ),
                            base64::URL_SAFE_NO_PAD,
                        ))
                            .into(),
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

    #[tracing::instrument(skip(key))]
    fn parse_servercurrentevent(
        key: &[u8],
        value: Vec<u8>,
    ) -> Result<(OutgoingKind, SendingEventType)> {
        // Appservices start with a plus
        Ok::<_, Error>(if key.starts_with(b"+") {
            let mut parts = key[1..].splitn(2, |&b| b == 0xff);

            let server = parts.next().expect("splitn always returns one element");
            let event = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let server = utils::string_from_bytes(server).map_err(|_| {
                Error::bad_database("Invalid server bytes in server_currenttransaction")
            })?;

            (
                OutgoingKind::Appservice(server),
                if value.is_empty() {
                    SendingEventType::Pdu(event.to_vec())
                } else {
                    SendingEventType::Edu(value)
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
                if value.is_empty() {
                    SendingEventType::Pdu(event.to_vec())
                } else {
                    SendingEventType::Edu(value)
                },
            )
        } else {
            let mut parts = key.splitn(2, |&b| b == 0xff);

            let server = parts.next().expect("splitn always returns one element");
            let event = parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid bytes in servercurrentpdus."))?;
            let server = utils::string_from_bytes(server).map_err(|_| {
                Error::bad_database("Invalid server bytes in server_currenttransaction")
            })?;

            (
                OutgoingKind::Normal(ServerName::parse(server).map_err(|_| {
                    Error::bad_database("Invalid server string in server_currenttransaction")
                })?),
                if value.is_empty() {
                    SendingEventType::Pdu(event.to_vec())
                } else {
                    SendingEventType::Edu(value)
                },
            )
        })
    }

    #[tracing::instrument(skip(self, destination, request))]
    pub async fn send_federation_request<T: OutgoingRequest>(
        &self,
        destination: &ServerName,
        request: T,
    ) -> Result<T::IncomingResponse>
    where
        T: Debug,
    {
        let permit = self.maximum_requests.acquire().await;
        let response = server_server::send_request(destination, request).await;
        drop(permit);

        response
    }

    #[tracing::instrument(skip(self, registration, request))]
    pub async fn send_appservice_request<T: OutgoingRequest>(
        &self,
        registration: serde_yaml::Value,
        request: T,
    ) -> Result<T::IncomingResponse>
    where
        T: Debug,
    {
        let permit = self.maximum_requests.acquire().await;
        let response = appservice_server::send_request(registration, request).await;
        drop(permit);

        response
    }
}
