mod data;

pub use data::Data;

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt::Debug,
    iter,
    sync::Arc,
    time::{Duration, Instant},
};

use crate::{
    api::{appservice_server, server_server},
    services,
    utils::{self, calculate_hash},
    Config, Error, PduEvent, Result,
};
use federation::transactions::send_transaction_message;
use futures_util::{stream::FuturesUnordered, StreamExt};

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
    sync::{mpsc, Semaphore},
};
use tracing::{error, warn};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum OutgoingKind {
    Appservice(String),
    Push(Box<UserId>, String), // user and pushkey
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
                p.extend_from_slice(user.as_bytes());
                p.push(0xff);
                p.extend_from_slice(pushkey.as_bytes());
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
    Pdu(Vec<u8>), // pduid
    Edu(Vec<u8>), // pdu json
}

pub struct Service {
    db: &'static dyn Data,

    /// The state for a given state hash.
    pub(super) maximum_requests: Arc<Semaphore>,
    pub sender: mpsc::UnboundedSender<(OutgoingKind, SendingEventType, Vec<u8>)>,
}

enum TransactionStatus {
    Running,
    Failed(u32, Instant), // number of times failed, time of last failure
    Retrying(u32),        // number of times failed
}

impl Service {
    pub fn build(db: &'static dyn Data, config: &Config) -> Arc<Self> {
        let (sender, receiver) = mpsc::unbounded_channel();

        let self1 = Arc::new(Self {
            db,
            sender,
            maximum_requests: Arc::new(Semaphore::new(config.max_concurrent_requests as usize)),
        });
        let self2 = Arc::clone(&self1);

        tokio::spawn(async move {
            self2.start_handler(receiver).await.unwrap();
        });

        self1
    }

    async fn start_handler(
        &self,
        mut receiver: mpsc::UnboundedReceiver<(OutgoingKind, SendingEventType, Vec<u8>)>,
    ) -> Result<()> {
        let mut futures = FuturesUnordered::new();

        let mut current_transaction_status = HashMap::<OutgoingKind, TransactionStatus>::new();

        // Retry requests we could not finish yet
        let mut initial_transactions = HashMap::<OutgoingKind, Vec<SendingEventType>>::new();

        for (key, outgoing_kind, event) in self.db.active_requests().filter_map(|r| r.ok()) {
            let entry = initial_transactions
                .entry(outgoing_kind.clone())
                .or_insert_with(Vec::new);

            if entry.len() > 30 {
                warn!(
                    "Dropping some current events: {:?} {:?} {:?}",
                    key, outgoing_kind, event
                );
                self.db.delete_active_request(key)?;
                continue;
            }

            entry.push(event);
        }

        for (outgoing_kind, events) in initial_transactions {
            current_transaction_status.insert(outgoing_kind.clone(), TransactionStatus::Running);
            futures.push(Self::handle_events(outgoing_kind.clone(), events));
        }

        loop {
            select! {
                Some(response) = futures.next() => {
                    match response {
                        Ok(outgoing_kind) => {
                            self.db.delete_all_active_requests_for(&outgoing_kind)?;

                            // Find events that have been added since starting the last request
                            let new_events = self.db.queued_requests(&outgoing_kind).filter_map(|r| r.ok()).take(30).collect::<Vec<_>>();

                            // TODO: find edus

                            if !new_events.is_empty() {
                                // Insert pdus we found
                                self.db.mark_as_active(&new_events)?;

                                futures.push(
                                    Self::handle_events(
                                        outgoing_kind.clone(),
                                        new_events.into_iter().map(|(event, _)| event).collect(),
                                    )
                                );
                            } else {
                                current_transaction_status.remove(&outgoing_kind);
                            }
                        }
                        Err((outgoing_kind, _)) => {
                            current_transaction_status.entry(outgoing_kind).and_modify(|e| *e = match e {
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
                Some((outgoing_kind, event, key)) = receiver.recv() => {
                    if let Ok(Some(events)) = self.select_events(
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

    #[tracing::instrument(skip(self, outgoing_kind, new_events, current_transaction_status))]
    fn select_events(
        &self,
        outgoing_kind: &OutgoingKind,
        new_events: Vec<(SendingEventType, Vec<u8>)>, // Events we want to send: event and full key
        current_transaction_status: &mut HashMap<OutgoingKind, TransactionStatus>,
    ) -> Result<Option<Vec<SendingEventType>>> {
        let mut retry = false;
        let mut allow = true;

        let entry = current_transaction_status.entry(outgoing_kind.clone());

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
            for (_, e) in self
                .db
                .active_requests_for(outgoing_kind)
                .filter_map(|r| r.ok())
            {
                events.push(e);
            }
        } else {
            self.db.mark_as_active(&new_events)?;
            for (e, _) in new_events {
                events.push(e);
            }

            if let OutgoingKind::Normal(server_name) = outgoing_kind {
                if let Ok((select_edus, last_count)) = self.select_edus(server_name) {
                    events.extend(select_edus.into_iter().map(SendingEventType::Edu));

                    self.db.set_latest_educount(server_name, last_count)?;
                }
            }
        }

        Ok(Some(events))
    }

    #[tracing::instrument(skip(self, server_name))]
    pub fn select_edus(&self, server_name: &ServerName) -> Result<(Vec<Vec<u8>>, u64)> {
        // u64: count of last edu
        let since = self.db.get_latest_educount(server_name)?;
        let mut events = Vec::new();
        let mut max_edu_count = since;
        let mut device_list_changes = HashSet::new();

        'outer: for room_id in services().rooms.state_cache.server_rooms(server_name) {
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
            for r in services()
                .rooms
                .edus
                .read_receipt
                .readreceipts_since(&room_id, since)
            {
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

    #[tracing::instrument(skip(self, pdu_id, user, pushkey))]
    pub fn send_push_pdu(&self, pdu_id: &[u8], user: &UserId, pushkey: String) -> Result<()> {
        let outgoing_kind = OutgoingKind::Push(user.to_owned(), pushkey);
        let event = SendingEventType::Pdu(pdu_id.to_owned());
        let keys = self.db.queue_requests(&[(&outgoing_kind, event.clone())])?;
        self.sender
            .send((outgoing_kind, event, keys.into_iter().next().unwrap()))
            .unwrap();

        Ok(())
    }

    #[tracing::instrument(skip(self, servers, pdu_id))]
    pub fn send_pdu<I: Iterator<Item = Box<ServerName>>>(
        &self,
        servers: I,
        pdu_id: &[u8],
    ) -> Result<()> {
        let requests = servers
            .into_iter()
            .map(|server| {
                (
                    OutgoingKind::Normal(server),
                    SendingEventType::Pdu(pdu_id.to_owned()),
                )
            })
            .collect::<Vec<_>>();
        let keys = self.db.queue_requests(
            &requests
                .iter()
                .map(|(o, e)| (o, e.clone()))
                .collect::<Vec<_>>(),
        )?;
        for ((outgoing_kind, event), key) in requests.into_iter().zip(keys) {
            self.sender
                .send((outgoing_kind.to_owned(), event, key))
                .unwrap();
        }

        Ok(())
    }

    #[tracing::instrument(skip(self, server, serialized))]
    pub fn send_reliable_edu(
        &self,
        server: &ServerName,
        serialized: Vec<u8>,
        id: u64,
    ) -> Result<()> {
        let outgoing_kind = OutgoingKind::Normal(server.to_owned());
        let event = SendingEventType::Edu(serialized);
        let keys = self.db.queue_requests(&[(&outgoing_kind, event.clone())])?;
        self.sender
            .send((outgoing_kind, event, keys.into_iter().next().unwrap()))
            .unwrap();

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn send_pdu_appservice(&self, appservice_id: String, pdu_id: Vec<u8>) -> Result<()> {
        let outgoing_kind = OutgoingKind::Appservice(appservice_id);
        let event = SendingEventType::Pdu(pdu_id);
        let keys = self.db.queue_requests(&[(&outgoing_kind, event.clone())])?;
        self.sender
            .send((outgoing_kind, event, keys.into_iter().next().unwrap()))
            .unwrap();

        Ok(())
    }

    /// Cleanup event data
    /// Used for instance after we remove an appservice registration
    ///
    #[tracing::instrument(skip(self))]
    pub fn cleanup_events(&self, appservice_id: String) -> Result<()> {
        self.db
            .delete_all_requests_for(&OutgoingKind::Appservice(appservice_id))?;

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
                            pdu_jsons.push(services().rooms.timeline
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
                .map(|_response| kind.clone())
                .map_err(|e| (kind, e));

                drop(permit);

                response
            }
            OutgoingKind::Push(userid, pushkey) => {
                let mut pdus = Vec::new();

                for event in &events {
                    match event {
                        SendingEventType::Pdu(pdu_id) => {
                            pdus.push(
                                services().rooms
                                    .timeline
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

                    let pusher = match services()
                        .pusher
                        .get_pusher(&userid, pushkey)
                        .map_err(|e| (OutgoingKind::Push(userid.clone(), pushkey.clone()), e))?
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
                        .and_then(|event| serde_json::from_str::<PushRulesEvent>(event.get()).ok())
                        .map(|ev: PushRulesEvent| ev.content.global)
                        .unwrap_or_else(|| push::Ruleset::server_default(&userid));

                    let unread: UInt = services()
                        .rooms
                        .user
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
                Ok(OutgoingKind::Push(userid.clone(), pushkey.clone()))
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
                                    .timeline
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
