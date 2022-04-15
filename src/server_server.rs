use crate::{
    client_server::{self, claim_keys_helper, get_keys_helper},
    database::{rooms::CompressedStateEvent, DatabaseGuard},
    pdu::EventHash,
    utils, Database, Error, PduEvent, Result, Ruma,
};
use axum::{response::IntoResponse, Json};
use futures_util::{stream::FuturesUnordered, StreamExt};
use get_profile_information::v1::ProfileField;
use http::header::{HeaderValue, AUTHORIZATION};
use regex::Regex;
use ruma::{
    api::{
        client::error::{Error as RumaError, ErrorKind},
        federation::{
            authorization::get_event_authorization,
            device::get_devices::{self, v1::UserDevice},
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                get_remote_server_keys, get_remote_server_keys_batch,
                get_remote_server_keys_batch::v2::QueryCriteria, get_server_keys,
                get_server_version, ServerSigningKeys, VerifyKey,
            },
            event::{get_event, get_missing_events, get_room_state, get_room_state_ids},
            keys::{claim_keys, get_keys},
            membership::{
                create_invite,
                create_join_event::{self, RoomState},
                prepare_join_event,
            },
            query::{get_profile_information, get_room_information},
            transactions::{
                edu::{DeviceListUpdateContent, DirectDeviceContent, Edu, SigningKeyUpdateContent},
                send_transaction_message,
            },
        },
        EndpointError, IncomingResponse, MatrixVersion, OutgoingRequest, OutgoingResponse,
        SendAccessToken,
    },
    directory::{IncomingFilter, IncomingRoomNetwork},
    events::{
        receipt::{ReceiptEvent, ReceiptEventContent},
        room::{
            create::RoomCreateEventContent,
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            member::{MembershipState, RoomMemberEventContent},
            server_acl::RoomServerAclEventContent,
        },
        RoomEventType, StateEventType,
    },
    int,
    receipt::ReceiptType,
    serde::{Base64, JsonObject, Raw},
    signatures::{CanonicalJsonObject, CanonicalJsonValue},
    state_res::{self, RoomVersion, StateMap},
    to_device::DeviceIdOrAllDevices,
    uint, EventId, MilliSecondsSinceUnixEpoch, RoomId, RoomVersionId, ServerName,
    ServerSigningKeyId,
};
use serde_json::value::{to_raw_value, RawValue as RawJsonValue};
use std::{
    collections::{btree_map, hash_map, BTreeMap, BTreeSet, HashMap, HashSet},
    fmt::Debug,
    future::Future,
    mem,
    net::{IpAddr, SocketAddr},
    ops::Deref,
    pin::Pin,
    sync::{Arc, RwLock, RwLockWriteGuard},
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::{MutexGuard, Semaphore};
use tracing::{debug, error, info, trace, warn};

/// Wraps either an literal IP address plus port, or a hostname plus complement
/// (colon-plus-port if it was specified).
///
/// Note: A `FedDest::Named` might contain an IP address in string form if there
/// was no port specified to construct a SocketAddr with.
///
/// # Examples:
/// ```rust
/// # use conduit::server_server::FedDest;
/// # fn main() -> Result<(), std::net::AddrParseError> {
/// FedDest::Literal("198.51.100.3:8448".parse()?);
/// FedDest::Literal("[2001:db8::4:5]:443".parse()?);
/// FedDest::Named("matrix.example.org".to_owned(), "".to_owned());
/// FedDest::Named("matrix.example.org".to_owned(), ":8448".to_owned());
/// FedDest::Named("198.51.100.5".to_owned(), "".to_owned());
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq)]
pub enum FedDest {
    Literal(SocketAddr),
    Named(String, String),
}

impl FedDest {
    fn into_https_string(self) -> String {
        match self {
            Self::Literal(addr) => format!("https://{}", addr),
            Self::Named(host, port) => format!("https://{}{}", host, port),
        }
    }

    fn into_uri_string(self) -> String {
        match self {
            Self::Literal(addr) => addr.to_string(),
            Self::Named(host, ref port) => host + port,
        }
    }

    fn hostname(&self) -> String {
        match &self {
            Self::Literal(addr) => addr.ip().to_string(),
            Self::Named(host, _) => host.clone(),
        }
    }

    fn port(&self) -> Option<u16> {
        match &self {
            Self::Literal(addr) => Some(addr.port()),
            Self::Named(_, port) => port[1..].parse().ok(),
        }
    }
}

#[tracing::instrument(skip(globals, request))]
pub(crate) async fn send_request<T: OutgoingRequest>(
    globals: &crate::database::globals::Globals,
    destination: &ServerName,
    request: T,
) -> Result<T::IncomingResponse>
where
    T: Debug,
{
    if !globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut write_destination_to_cache = false;

    let cached_result = globals
        .actual_destination_cache
        .read()
        .unwrap()
        .get(destination)
        .cloned();

    let (actual_destination, host) = if let Some(result) = cached_result {
        result
    } else {
        write_destination_to_cache = true;

        let result = find_actual_destination(globals, destination).await;

        (result.0, result.1.into_uri_string())
    };

    let actual_destination_str = actual_destination.clone().into_https_string();

    let mut http_request = request
        .try_into_http_request::<Vec<u8>>(
            &actual_destination_str,
            SendAccessToken::IfRequired(""),
            &[MatrixVersion::V1_0],
        )
        .map_err(|e| {
            warn!(
                "Failed to find destination {}: {}",
                actual_destination_str, e
            );
            Error::BadServerResponse("Invalid destination")
        })?;

    let mut request_map = serde_json::Map::new();

    if !http_request.body().is_empty() {
        request_map.insert(
            "content".to_owned(),
            serde_json::from_slice(http_request.body())
                .expect("body is valid json, we just created it"),
        );
    };

    request_map.insert("method".to_owned(), T::METADATA.method.to_string().into());
    request_map.insert(
        "uri".to_owned(),
        http_request
            .uri()
            .path_and_query()
            .expect("all requests have a path")
            .to_string()
            .into(),
    );
    request_map.insert("origin".to_owned(), globals.server_name().as_str().into());
    request_map.insert("destination".to_owned(), destination.as_str().into());

    let mut request_json =
        serde_json::from_value(request_map.into()).expect("valid JSON is valid BTreeMap");

    ruma::signatures::sign_json(
        globals.server_name().as_str(),
        globals.keypair(),
        &mut request_json,
    )
    .expect("our request json is what ruma expects");

    let request_json: serde_json::Map<String, serde_json::Value> =
        serde_json::from_slice(&serde_json::to_vec(&request_json).unwrap()).unwrap();

    let signatures = request_json["signatures"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| {
            v.as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| (k, v.as_str().unwrap()))
        });

    for signature_server in signatures {
        for s in signature_server {
            http_request.headers_mut().insert(
                AUTHORIZATION,
                HeaderValue::from_str(&format!(
                    "X-Matrix origin={},key=\"{}\",sig=\"{}\"",
                    globals.server_name(),
                    s.0,
                    s.1
                ))
                .unwrap(),
            );
        }
    }

    let reqwest_request = reqwest::Request::try_from(http_request)
        .expect("all http requests are valid reqwest requests");

    let url = reqwest_request.url().clone();

    let response = globals.federation_client().execute(reqwest_request).await;

    match response {
        Ok(mut response) => {
            // reqwest::Response -> http::Response conversion
            let status = response.status();
            let mut http_response_builder = http::Response::builder()
                .status(status)
                .version(response.version());
            mem::swap(
                response.headers_mut(),
                http_response_builder
                    .headers_mut()
                    .expect("http::response::Builder is usable"),
            );

            let body = response.bytes().await.unwrap_or_else(|e| {
                warn!("server error {}", e);
                Vec::new().into()
            }); // TODO: handle timeout

            if status != 200 {
                warn!(
                    "{} {}: {}",
                    url,
                    status,
                    String::from_utf8_lossy(&body)
                        .lines()
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }

            let http_response = http_response_builder
                .body(body)
                .expect("reqwest body is valid http body");

            if status == 200 {
                let response = T::IncomingResponse::try_from_http_response(http_response);
                if response.is_ok() && write_destination_to_cache {
                    globals.actual_destination_cache.write().unwrap().insert(
                        Box::<ServerName>::from(destination),
                        (actual_destination, host),
                    );
                }

                response.map_err(|e| {
                    warn!(
                        "Invalid 200 response from {} on: {} {}",
                        &destination, url, e
                    );
                    Error::BadServerResponse("Server returned bad 200 response.")
                })
            } else {
                Err(Error::FederationError(
                    destination.to_owned(),
                    RumaError::try_from_http_response(http_response).map_err(|e| {
                        warn!(
                            "Invalid {} response from {} on: {} {}",
                            status, &destination, url, e
                        );
                        Error::BadServerResponse("Server returned bad error response.")
                    })?,
                ))
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn get_ip_with_port(destination_str: &str) -> Option<FedDest> {
    if let Ok(destination) = destination_str.parse::<SocketAddr>() {
        Some(FedDest::Literal(destination))
    } else if let Ok(ip_addr) = destination_str.parse::<IpAddr>() {
        Some(FedDest::Literal(SocketAddr::new(ip_addr, 8448)))
    } else {
        None
    }
}

fn add_port_to_hostname(destination_str: &str) -> FedDest {
    let (host, port) = match destination_str.find(':') {
        None => (destination_str, ":8448"),
        Some(pos) => destination_str.split_at(pos),
    };
    FedDest::Named(host.to_owned(), port.to_owned())
}

/// Returns: actual_destination, host header
/// Implemented according to the specification at https://matrix.org/docs/spec/server_server/r0.1.4#resolving-server-names
/// Numbers in comments below refer to bullet points in linked section of specification
#[tracing::instrument(skip(globals))]
async fn find_actual_destination(
    globals: &crate::database::globals::Globals,
    destination: &'_ ServerName,
) -> (FedDest, FedDest) {
    let destination_str = destination.as_str().to_owned();
    let mut hostname = destination_str.clone();
    let actual_destination = match get_ip_with_port(&destination_str) {
        Some(host_port) => {
            // 1: IP literal with provided or default port
            host_port
        }
        None => {
            if let Some(pos) = destination_str.find(':') {
                // 2: Hostname with included port
                let (host, port) = destination_str.split_at(pos);
                FedDest::Named(host.to_owned(), port.to_owned())
            } else {
                match request_well_known(globals, destination.as_str()).await {
                    // 3: A .well-known file is available
                    Some(delegated_hostname) => {
                        hostname = add_port_to_hostname(&delegated_hostname).into_uri_string();
                        match get_ip_with_port(&delegated_hostname) {
                            Some(host_and_port) => host_and_port, // 3.1: IP literal in .well-known file
                            None => {
                                if let Some(pos) = delegated_hostname.find(':') {
                                    // 3.2: Hostname with port in .well-known file
                                    let (host, port) = delegated_hostname.split_at(pos);
                                    FedDest::Named(host.to_owned(), port.to_owned())
                                } else {
                                    // Delegated hostname has no port in this branch
                                    if let Some(hostname_override) =
                                        query_srv_record(globals, &delegated_hostname).await
                                    {
                                        // 3.3: SRV lookup successful
                                        let force_port = hostname_override.port();

                                        if let Ok(override_ip) = globals
                                            .dns_resolver()
                                            .lookup_ip(hostname_override.hostname())
                                            .await
                                        {
                                            globals.tls_name_override.write().unwrap().insert(
                                                delegated_hostname.clone(),
                                                (
                                                    override_ip.iter().collect(),
                                                    force_port.unwrap_or(8448),
                                                ),
                                            );
                                        } else {
                                            warn!("Using SRV record, but could not resolve to IP");
                                        }

                                        if let Some(port) = force_port {
                                            FedDest::Named(delegated_hostname, format!(":{}", port))
                                        } else {
                                            add_port_to_hostname(&delegated_hostname)
                                        }
                                    } else {
                                        // 3.4: No SRV records, just use the hostname from .well-known
                                        add_port_to_hostname(&delegated_hostname)
                                    }
                                }
                            }
                        }
                    }
                    // 4: No .well-known or an error occured
                    None => {
                        match query_srv_record(globals, &destination_str).await {
                            // 4: SRV record found
                            Some(hostname_override) => {
                                let force_port = hostname_override.port();

                                if let Ok(override_ip) = globals
                                    .dns_resolver()
                                    .lookup_ip(hostname_override.hostname())
                                    .await
                                {
                                    globals.tls_name_override.write().unwrap().insert(
                                        hostname.clone(),
                                        (override_ip.iter().collect(), force_port.unwrap_or(8448)),
                                    );
                                } else {
                                    warn!("Using SRV record, but could not resolve to IP");
                                }

                                if let Some(port) = force_port {
                                    FedDest::Named(hostname.clone(), format!(":{}", port))
                                } else {
                                    add_port_to_hostname(&hostname)
                                }
                            }
                            // 5: No SRV record found
                            None => add_port_to_hostname(&destination_str),
                        }
                    }
                }
            }
        }
    };

    // Can't use get_ip_with_port here because we don't want to add a port
    // to an IP address if it wasn't specified
    let hostname = if let Ok(addr) = hostname.parse::<SocketAddr>() {
        FedDest::Literal(addr)
    } else if let Ok(addr) = hostname.parse::<IpAddr>() {
        FedDest::Named(addr.to_string(), ":8448".to_owned())
    } else if let Some(pos) = hostname.find(':') {
        let (host, port) = hostname.split_at(pos);
        FedDest::Named(host.to_owned(), port.to_owned())
    } else {
        FedDest::Named(hostname, ":8448".to_owned())
    };
    (actual_destination, hostname)
}

#[tracing::instrument(skip(globals))]
async fn query_srv_record(
    globals: &crate::database::globals::Globals,
    hostname: &'_ str,
) -> Option<FedDest> {
    if let Ok(Some(host_port)) = globals
        .dns_resolver()
        .srv_lookup(format!("_matrix._tcp.{}", hostname))
        .await
        .map(|srv| {
            srv.iter().next().map(|result| {
                FedDest::Named(
                    result.target().to_string().trim_end_matches('.').to_owned(),
                    format!(":{}", result.port()),
                )
            })
        })
    {
        Some(host_port)
    } else {
        None
    }
}

#[tracing::instrument(skip(globals))]
async fn request_well_known(
    globals: &crate::database::globals::Globals,
    destination: &str,
) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(
        &globals
            .default_client()
            .get(&format!(
                "https://{}/.well-known/matrix/server",
                destination
            ))
            .send()
            .await
            .ok()?
            .text()
            .await
            .ok()?,
    )
    .ok()?;
    Some(body.get("m.server")?.as_str()?.to_owned())
}

/// # `GET /_matrix/federation/v1/version`
///
/// Get version information on this server.
pub async fn get_server_version_route(
    db: DatabaseGuard,
    _body: Ruma<get_server_version::v1::Request>,
) -> Result<get_server_version::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    Ok(get_server_version::v1::Response {
        server: Some(get_server_version::v1::Server {
            name: Some("Conduit".to_owned()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
    })
}

/// # `GET /_matrix/key/v2/server`
///
/// Gets the public signing keys of this server.
///
/// - Matrix does not support invalidating public keys, so the key returned by this will be valid
/// forever.
// Response type for this endpoint is Json because we need to calculate a signature for the response
pub async fn get_server_keys_route(db: DatabaseGuard) -> Result<impl IntoResponse> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut verify_keys: BTreeMap<Box<ServerSigningKeyId>, VerifyKey> = BTreeMap::new();
    verify_keys.insert(
        format!("ed25519:{}", db.globals.keypair().version())
            .try_into()
            .expect("found invalid server signing keys in DB"),
        VerifyKey {
            key: Base64::new(db.globals.keypair().public_key().to_vec()),
        },
    );
    let mut response = serde_json::from_slice(
        get_server_keys::v2::Response {
            server_key: Raw::new(&ServerSigningKeys {
                server_name: db.globals.server_name().to_owned(),
                verify_keys,
                old_verify_keys: BTreeMap::new(),
                signatures: BTreeMap::new(),
                valid_until_ts: MilliSecondsSinceUnixEpoch::from_system_time(
                    SystemTime::now() + Duration::from_secs(86400 * 7),
                )
                .expect("time is valid"),
            })
            .expect("static conversion, no errors"),
        }
        .try_into_http_response::<Vec<u8>>()
        .unwrap()
        .body(),
    )
    .unwrap();

    ruma::signatures::sign_json(
        db.globals.server_name().as_str(),
        db.globals.keypair(),
        &mut response,
    )
    .unwrap();

    Ok(Json(response))
}

/// # `GET /_matrix/key/v2/server/{keyId}`
///
/// Gets the public signing keys of this server.
///
/// - Matrix does not support invalidating public keys, so the key returned by this will be valid
/// forever.
pub async fn get_server_keys_deprecated_route(db: DatabaseGuard) -> impl IntoResponse {
    get_server_keys_route(db).await
}

/// # `POST /_matrix/federation/v1/publicRooms`
///
/// Lists the public rooms on this server.
pub async fn get_public_rooms_filtered_route(
    db: DatabaseGuard,
    body: Ruma<get_public_rooms_filtered::v1::IncomingRequest>,
) -> Result<get_public_rooms_filtered::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let response = client_server::get_public_rooms_filtered_helper(
        &db,
        None,
        body.limit,
        body.since.as_deref(),
        &body.filter,
        &body.room_network,
    )
    .await?;

    Ok(get_public_rooms_filtered::v1::Response {
        chunk: response.chunk,
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    })
}

/// # `GET /_matrix/federation/v1/publicRooms`
///
/// Lists the public rooms on this server.
pub async fn get_public_rooms_route(
    db: DatabaseGuard,
    body: Ruma<get_public_rooms::v1::IncomingRequest>,
) -> Result<get_public_rooms::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let response = client_server::get_public_rooms_filtered_helper(
        &db,
        None,
        body.limit,
        body.since.as_deref(),
        &IncomingFilter::default(),
        &IncomingRoomNetwork::Matrix,
    )
    .await?;

    Ok(get_public_rooms::v1::Response {
        chunk: response.chunk,
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    })
}

/// # `PUT /_matrix/federation/v1/send/{txnId}`
///
/// Push EDUs and PDUs to this server.
pub async fn send_transaction_message_route(
    db: DatabaseGuard,
    body: Ruma<send_transaction_message::v1::IncomingRequest>,
) -> Result<send_transaction_message::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    let mut resolved_map = BTreeMap::new();

    let pub_key_map = RwLock::new(BTreeMap::new());

    // This is all the auth_events that have been recursively fetched so they don't have to be
    // deserialized over and over again.
    // TODO: make this persist across requests but not in a DB Tree (in globals?)
    // TODO: This could potentially also be some sort of trie (suffix tree) like structure so
    // that once an auth event is known it would know (using indexes maybe) all of the auth
    // events that it references.
    // let mut auth_cache = EventMap::new();

    for pdu in &body.pdus {
        // We do not add the event_id field to the pdu here because of signature and hashes checks
        let (event_id, value) = match crate::pdu::gen_event_id_canonical_json(pdu, &db) {
            Ok(t) => t,
            Err(_) => {
                // Event could not be converted to canonical json
                continue;
            }
        };

        // 0. Check the server is in the room
        let room_id = match value
            .get("room_id")
            .and_then(|id| RoomId::parse(id.as_str()?).ok())
        {
            Some(id) => id,
            None => {
                // Event is invalid
                resolved_map.insert(event_id, Err("Event needs a valid RoomId.".to_owned()));
                continue;
            }
        };

        acl_check(sender_servername, &room_id, &db)?;

        let mutex = Arc::clone(
            db.globals
                .roomid_mutex_federation
                .write()
                .unwrap()
                .entry(room_id.clone())
                .or_default(),
        );
        let mutex_lock = mutex.lock().await;
        let start_time = Instant::now();
        resolved_map.insert(
            event_id.clone(),
            handle_incoming_pdu(
                sender_servername,
                &event_id,
                &room_id,
                value,
                true,
                &db,
                &pub_key_map,
            )
            .await
            .map(|_| ()),
        );
        drop(mutex_lock);

        let elapsed = start_time.elapsed();
        warn!(
            "Handling transaction of event {} took {}m{}s",
            event_id,
            elapsed.as_secs() / 60,
            elapsed.as_secs() % 60
        );
    }

    for pdu in &resolved_map {
        if let Err(e) = pdu.1 {
            if e != "Room is unknown to this server." {
                warn!("Incoming PDU failed {:?}", pdu);
            }
        }
    }

    for edu in body
        .edus
        .iter()
        .filter_map(|edu| serde_json::from_str::<Edu>(edu.json().get()).ok())
    {
        match edu {
            Edu::Presence(_) => {}
            Edu::Receipt(receipt) => {
                for (room_id, room_updates) in receipt.receipts {
                    for (user_id, user_updates) in room_updates.read {
                        if let Some((event_id, _)) = user_updates
                            .event_ids
                            .iter()
                            .filter_map(|id| {
                                db.rooms.get_pdu_count(id).ok().flatten().map(|r| (id, r))
                            })
                            .max_by_key(|(_, count)| *count)
                        {
                            let mut user_receipts = BTreeMap::new();
                            user_receipts.insert(user_id.clone(), user_updates.data);

                            let mut receipts = BTreeMap::new();
                            receipts.insert(ReceiptType::Read, user_receipts);

                            let mut receipt_content = BTreeMap::new();
                            receipt_content.insert(event_id.to_owned(), receipts);

                            let event = ReceiptEvent {
                                content: ReceiptEventContent(receipt_content),
                                room_id: room_id.clone(),
                            };
                            db.rooms.edus.readreceipt_update(
                                &user_id,
                                &room_id,
                                event,
                                &db.globals,
                            )?;
                        } else {
                            // TODO fetch missing events
                            debug!("No known event ids in read receipt: {:?}", user_updates);
                        }
                    }
                }
            }
            Edu::Typing(typing) => {
                if db.rooms.is_joined(&typing.user_id, &typing.room_id)? {
                    if typing.typing {
                        db.rooms.edus.typing_add(
                            &typing.user_id,
                            &typing.room_id,
                            3000 + utils::millis_since_unix_epoch(),
                            &db.globals,
                        )?;
                    } else {
                        db.rooms.edus.typing_remove(
                            &typing.user_id,
                            &typing.room_id,
                            &db.globals,
                        )?;
                    }
                }
            }
            Edu::DeviceListUpdate(DeviceListUpdateContent { user_id, .. }) => {
                db.users
                    .mark_device_key_update(&user_id, &db.rooms, &db.globals)?;
            }
            Edu::DirectToDevice(DirectDeviceContent {
                sender,
                ev_type,
                message_id,
                messages,
            }) => {
                // Check if this is a new transaction id
                if db
                    .transaction_ids
                    .existing_txnid(&sender, None, &message_id)?
                    .is_some()
                {
                    continue;
                }

                for (target_user_id, map) in &messages {
                    for (target_device_id_maybe, event) in map {
                        match target_device_id_maybe {
                            DeviceIdOrAllDevices::DeviceId(target_device_id) => {
                                db.users.add_to_device_event(
                                    &sender,
                                    target_user_id,
                                    target_device_id,
                                    &ev_type.to_string(),
                                    event.deserialize_as().map_err(|_| {
                                        Error::BadRequest(
                                            ErrorKind::InvalidParam,
                                            "Event is invalid",
                                        )
                                    })?,
                                    &db.globals,
                                )?
                            }

                            DeviceIdOrAllDevices::AllDevices => {
                                for target_device_id in db.users.all_device_ids(target_user_id) {
                                    db.users.add_to_device_event(
                                        &sender,
                                        target_user_id,
                                        &target_device_id?,
                                        &ev_type.to_string(),
                                        event.deserialize_as().map_err(|_| {
                                            Error::BadRequest(
                                                ErrorKind::InvalidParam,
                                                "Event is invalid",
                                            )
                                        })?,
                                        &db.globals,
                                    )?;
                                }
                            }
                        }
                    }
                }

                // Save transaction id with empty data
                db.transaction_ids
                    .add_txnid(&sender, None, &message_id, &[])?;
            }
            Edu::SigningKeyUpdate(SigningKeyUpdateContent {
                user_id,
                master_key,
                self_signing_key,
            }) => {
                if user_id.server_name() != sender_servername {
                    continue;
                }
                if let Some(master_key) = master_key {
                    db.users.add_cross_signing_keys(
                        &user_id,
                        &master_key,
                        &self_signing_key,
                        &None,
                        &db.rooms,
                        &db.globals,
                    )?;
                }
            }
            Edu::_Custom(_) => {}
        }
    }

    db.flush()?;

    Ok(send_transaction_message::v1::Response { pdus: resolved_map })
}

/// An async function that can recursively call itself.
type AsyncRecursiveType<'a, T> = Pin<Box<dyn Future<Output = T> + 'a + Send>>;

/// When receiving an event one needs to:
/// 0. Check the server is in the room
/// 1. Skip the PDU if we already know about it
/// 2. Check signatures, otherwise drop
/// 3. Check content hash, redact if doesn't match
/// 4. Fetch any missing auth events doing all checks listed here starting at 1. These are not
///    timeline events
/// 5. Reject "due to auth events" if can't get all the auth events or some of the auth events are
///    also rejected "due to auth events"
/// 6. Reject "due to auth events" if the event doesn't pass auth based on the auth events
/// 7. Persist this event as an outlier
/// 8. If not timeline event: stop
/// 9. Fetch any missing prev events doing all checks listed here starting at 1. These are timeline
///    events
/// 10. Fetch missing state and auth chain events by calling /state_ids at backwards extremities
///     doing all the checks in this list starting at 1. These are not timeline events
/// 11. Check the auth of the event passes based on the state of the event
/// 12. Ensure that the state is derived from the previous current state (i.e. we calculated by
///     doing state res where one of the inputs was a previously trusted set of state, don't just
///     trust a set of state we got from a remote)
/// 13. Check if the event passes auth based on the "current state" of the room, if not "soft fail"
///     it
/// 14. Use state resolution to find new room state
// We use some AsyncRecursiveType hacks here so we can call this async funtion recursively
#[tracing::instrument(skip(value, is_timeline_event, db, pub_key_map))]
pub(crate) async fn handle_incoming_pdu<'a>(
    origin: &'a ServerName,
    event_id: &'a EventId,
    room_id: &'a RoomId,
    value: BTreeMap<String, CanonicalJsonValue>,
    is_timeline_event: bool,
    db: &'a Database,
    pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
) -> Result<Option<Vec<u8>>, String> {
    match db.rooms.exists(room_id) {
        Ok(true) => {}
        _ => {
            return Err("Room is unknown to this server.".to_owned());
        }
    }

    // 1. Skip the PDU if we already have it as a timeline event
    if let Ok(Some(pdu_id)) = db.rooms.get_pdu_id(event_id) {
        return Ok(Some(pdu_id.to_vec()));
    }

    let create_event = db
        .rooms
        .room_state_get(room_id, &StateEventType::RoomCreate, "")
        .map_err(|_| "Failed to ask database for event.".to_owned())?
        .ok_or_else(|| "Failed to find create event in db.".to_owned())?;

    let first_pdu_in_room = db
        .rooms
        .first_pdu_in_room(room_id)
        .map_err(|_| "Error loading first room event.".to_owned())?
        .expect("Room exists");

    let (incoming_pdu, val) = handle_outlier_pdu(
        origin,
        &create_event,
        event_id,
        room_id,
        value,
        db,
        pub_key_map,
    )
    .await?;

    // 8. if not timeline event: stop
    if !is_timeline_event {
        return Ok(None);
    }

    if incoming_pdu.origin_server_ts < first_pdu_in_room.origin_server_ts {
        return Ok(None);
    }

    // 9. Fetch any missing prev events doing all checks listed here starting at 1. These are timeline events
    let mut graph: HashMap<Arc<EventId>, _> = HashMap::new();
    let mut eventid_info = HashMap::new();
    let mut todo_outlier_stack: Vec<Arc<EventId>> = incoming_pdu.prev_events.clone();

    let mut amount = 0;

    while let Some(prev_event_id) = todo_outlier_stack.pop() {
        if let Some((pdu, json_opt)) = fetch_and_handle_outliers(
            db,
            origin,
            &[prev_event_id.clone()],
            &create_event,
            room_id,
            pub_key_map,
        )
        .await
        .pop()
        {
            if amount > 100 {
                // Max limit reached
                warn!("Max prev event limit reached!");
                graph.insert(prev_event_id.clone(), HashSet::new());
                continue;
            }

            if let Some(json) =
                json_opt.or_else(|| db.rooms.get_outlier_pdu_json(&prev_event_id).ok().flatten())
            {
                if pdu.origin_server_ts > first_pdu_in_room.origin_server_ts {
                    amount += 1;
                    for prev_prev in &pdu.prev_events {
                        if !graph.contains_key(prev_prev) {
                            todo_outlier_stack.push(dbg!(prev_prev.clone()));
                        }
                    }

                    graph.insert(
                        prev_event_id.clone(),
                        pdu.prev_events.iter().cloned().collect(),
                    );
                } else {
                    // Time based check failed
                    graph.insert(prev_event_id.clone(), HashSet::new());
                }

                eventid_info.insert(prev_event_id.clone(), (pdu, json));
            } else {
                // Get json failed
                graph.insert(prev_event_id.clone(), HashSet::new());
            }
        } else {
            // Fetch and handle failed
            graph.insert(prev_event_id.clone(), HashSet::new());
        }
    }

    let sorted = state_res::lexicographical_topological_sort(dbg!(&graph), |event_id| {
        // This return value is the key used for sorting events,
        // events are then sorted by power level, time,
        // and lexically by event_id.
        println!("{}", event_id);
        Ok((
            int!(0),
            MilliSecondsSinceUnixEpoch(
                eventid_info
                    .get(event_id)
                    .map_or_else(|| uint!(0), |info| info.0.origin_server_ts),
            ),
        ))
    })
    .map_err(|_| "Error sorting prev events".to_owned())?;

    let mut errors = 0;
    for prev_id in dbg!(sorted) {
        if errors >= 5 {
            break;
        }
        if let Some((pdu, json)) = eventid_info.remove(&*prev_id) {
            if pdu.origin_server_ts < first_pdu_in_room.origin_server_ts {
                continue;
            }

            let start_time = Instant::now();
            let event_id = pdu.event_id.clone();
            if let Err(e) = upgrade_outlier_to_timeline_pdu(
                pdu,
                json,
                &create_event,
                origin,
                db,
                room_id,
                pub_key_map,
            )
            .await
            {
                errors += 1;
                warn!("Prev event {} failed: {}", event_id, e);
            }
            let elapsed = start_time.elapsed();
            warn!(
                "Handling prev event {} took {}m{}s",
                event_id,
                elapsed.as_secs() / 60,
                elapsed.as_secs() % 60
            );
        }
    }

    upgrade_outlier_to_timeline_pdu(
        incoming_pdu,
        val,
        &create_event,
        origin,
        db,
        room_id,
        pub_key_map,
    )
    .await
}

#[tracing::instrument(skip_all)]
fn handle_outlier_pdu<'a>(
    origin: &'a ServerName,
    create_event: &'a PduEvent,
    event_id: &'a EventId,
    room_id: &'a RoomId,
    value: BTreeMap<String, CanonicalJsonValue>,
    db: &'a Database,
    pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
) -> AsyncRecursiveType<'a, Result<(Arc<PduEvent>, BTreeMap<String, CanonicalJsonValue>), String>> {
    Box::pin(async move {
        // TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json

        // We go through all the signatures we see on the value and fetch the corresponding signing
        // keys
        fetch_required_signing_keys(&value, pub_key_map, db)
            .await
            .map_err(|e| e.to_string())?;

        // 2. Check signatures, otherwise drop
        // 3. check content hash, redact if doesn't match

        let create_event_content: RoomCreateEventContent =
            serde_json::from_str(create_event.content.get()).map_err(|e| {
                warn!("Invalid create event: {}", e);
                "Invalid create event in db.".to_owned()
            })?;

        let room_version_id = &create_event_content.room_version;
        let room_version = RoomVersion::new(room_version_id).expect("room version is supported");

        let mut val = match ruma::signatures::verify_event(
            &*pub_key_map.read().map_err(|_| "RwLock is poisoned.")?,
            &value,
            room_version_id,
        ) {
            Err(e) => {
                // Drop
                warn!("Dropping bad event {}: {}", event_id, e);
                return Err("Signature verification failed".to_owned());
            }
            Ok(ruma::signatures::Verified::Signatures) => {
                // Redact
                warn!("Calculated hash does not match: {}", event_id);
                match ruma::signatures::redact(&value, room_version_id) {
                    Ok(obj) => obj,
                    Err(_) => return Err("Redaction failed".to_owned()),
                }
            }
            Ok(ruma::signatures::Verified::All) => value,
        };

        // Now that we have checked the signature and hashes we can add the eventID and convert
        // to our PduEvent type
        val.insert(
            "event_id".to_owned(),
            CanonicalJsonValue::String(event_id.as_str().to_owned()),
        );
        let incoming_pdu = serde_json::from_value::<PduEvent>(
            serde_json::to_value(&val).expect("CanonicalJsonObj is a valid JsonValue"),
        )
        .map_err(|_| "Event is not a valid PDU.".to_owned())?;

        // 4. fetch any missing auth events doing all checks listed here starting at 1. These are not timeline events
        // 5. Reject "due to auth events" if can't get all the auth events or some of the auth events are also rejected "due to auth events"
        // EDIT: Step 5 is not applied anymore because it failed too often
        warn!("Fetching auth events for {}", incoming_pdu.event_id);
        fetch_and_handle_outliers(
            db,
            origin,
            &incoming_pdu
                .auth_events
                .iter()
                .map(|x| Arc::from(&**x))
                .collect::<Vec<_>>(),
            create_event,
            room_id,
            pub_key_map,
        )
        .await;

        // 6. Reject "due to auth events" if the event doesn't pass auth based on the auth events
        debug!(
            "Auth check for {} based on auth events",
            incoming_pdu.event_id
        );

        // Build map of auth events
        let mut auth_events = HashMap::new();
        for id in &incoming_pdu.auth_events {
            let auth_event = match db.rooms.get_pdu(id).map_err(|e| e.to_string())? {
                Some(e) => e,
                None => {
                    warn!("Could not find auth event {}", id);
                    continue;
                }
            };

            match auth_events.entry((
                auth_event.kind.to_string().into(),
                auth_event
                    .state_key
                    .clone()
                    .expect("all auth events have state keys"),
            )) {
                hash_map::Entry::Vacant(v) => {
                    v.insert(auth_event);
                }
                hash_map::Entry::Occupied(_) => {
                    return Err(
                        "Auth event's type and state_key combination exists multiple times."
                            .to_owned(),
                    )
                }
            }
        }

        // The original create event must be in the auth events
        if auth_events
            .get(&(StateEventType::RoomCreate, "".to_owned()))
            .map(|a| a.as_ref())
            != Some(create_event)
        {
            return Err("Incoming event refers to wrong create event.".to_owned());
        }

        if !state_res::event_auth::auth_check(
            &room_version,
            &incoming_pdu,
            None::<PduEvent>, // TODO: third party invite
            |k, s| auth_events.get(&(k.to_owned(), s.to_owned())),
        )
        .map_err(|_e| "Auth check failed".to_owned())?
        {
            return Err("Event has failed auth check with auth events.".to_owned());
        }

        debug!("Validation successful.");

        // 7. Persist the event as an outlier.
        db.rooms
            .add_pdu_outlier(&incoming_pdu.event_id, &val)
            .map_err(|_| "Failed to add pdu as outlier.".to_owned())?;
        debug!("Added pdu as outlier.");

        Ok((Arc::new(incoming_pdu), val))
    })
}

#[tracing::instrument(skip_all)]
async fn upgrade_outlier_to_timeline_pdu(
    incoming_pdu: Arc<PduEvent>,
    val: BTreeMap<String, CanonicalJsonValue>,
    create_event: &PduEvent,
    origin: &ServerName,
    db: &Database,
    room_id: &RoomId,
    pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
) -> Result<Option<Vec<u8>>, String> {
    if let Ok(Some(pduid)) = db.rooms.get_pdu_id(&incoming_pdu.event_id) {
        return Ok(Some(pduid));
    }

    if db
        .rooms
        .is_event_soft_failed(&incoming_pdu.event_id)
        .map_err(|_| "Failed to ask db for soft fail".to_owned())?
    {
        return Err("Event has been soft failed".into());
    }

    let create_event_content: RoomCreateEventContent =
        serde_json::from_str(create_event.content.get()).map_err(|e| {
            warn!("Invalid create event: {}", e);
            "Invalid create event in db.".to_owned()
        })?;

    let room_version_id = &create_event_content.room_version;
    let room_version = RoomVersion::new(room_version_id).expect("room version is supported");

    // 10. Fetch missing state and auth chain events by calling /state_ids at backwards extremities
    //     doing all the checks in this list starting at 1. These are not timeline events.

    // TODO: if we know the prev_events of the incoming event we can avoid the request and build
    // the state from a known point and resolve if > 1 prev_event

    debug!("Requesting state at event.");
    let mut state_at_incoming_event = None;

    if incoming_pdu.prev_events.len() == 1 {
        let prev_event = &*incoming_pdu.prev_events[0];
        let prev_event_sstatehash = db
            .rooms
            .pdu_shortstatehash(prev_event)
            .map_err(|_| "Failed talking to db".to_owned())?;

        let state =
            prev_event_sstatehash.map(|shortstatehash| db.rooms.state_full_ids(shortstatehash));

        if let Some(Ok(mut state)) = state {
            warn!("Using cached state");
            let prev_pdu =
                db.rooms.get_pdu(prev_event).ok().flatten().ok_or_else(|| {
                    "Could not find prev event, but we know the state.".to_owned()
                })?;

            if let Some(state_key) = &prev_pdu.state_key {
                let shortstatekey = db
                    .rooms
                    .get_or_create_shortstatekey(
                        &prev_pdu.kind.to_string().into(),
                        state_key,
                        &db.globals,
                    )
                    .map_err(|_| "Failed to create shortstatekey.".to_owned())?;

                state.insert(shortstatekey, Arc::from(prev_event));
                // Now it's the state after the pdu
            }

            state_at_incoming_event = Some(state);
        }
    } else {
        warn!("Calculating state at event using state res");
        let mut extremity_sstatehashes = HashMap::new();

        let mut okay = true;
        for prev_eventid in &incoming_pdu.prev_events {
            let prev_event = if let Ok(Some(pdu)) = db.rooms.get_pdu(prev_eventid) {
                pdu
            } else {
                okay = false;
                break;
            };

            let sstatehash = if let Ok(Some(s)) = db.rooms.pdu_shortstatehash(prev_eventid) {
                s
            } else {
                okay = false;
                break;
            };

            extremity_sstatehashes.insert(sstatehash, prev_event);
        }

        if okay {
            let mut fork_states = Vec::with_capacity(extremity_sstatehashes.len());
            let mut auth_chain_sets = Vec::with_capacity(extremity_sstatehashes.len());

            for (sstatehash, prev_event) in extremity_sstatehashes {
                let mut leaf_state: BTreeMap<_, _> = db
                    .rooms
                    .state_full_ids(sstatehash)
                    .map_err(|_| "Failed to ask db for room state.".to_owned())?;

                if let Some(state_key) = &prev_event.state_key {
                    let shortstatekey = db
                        .rooms
                        .get_or_create_shortstatekey(
                            &prev_event.kind.to_string().into(),
                            state_key,
                            &db.globals,
                        )
                        .map_err(|_| "Failed to create shortstatekey.".to_owned())?;
                    leaf_state.insert(shortstatekey, Arc::from(&*prev_event.event_id));
                    // Now it's the state after the pdu
                }

                let mut state = StateMap::with_capacity(leaf_state.len());
                let mut starting_events = Vec::with_capacity(leaf_state.len());

                for (k, id) in leaf_state {
                    if let Ok((ty, st_key)) = db.rooms.get_statekey_from_short(k) {
                        state.insert((ty, st_key), id.clone());
                    } else {
                        warn!("Failed to get_statekey_from_short.");
                    }
                    starting_events.push(id);
                }

                auth_chain_sets.push(
                    get_auth_chain(room_id, starting_events, db)
                        .map_err(|_| "Failed to load auth chain.".to_owned())?
                        .collect(),
                );

                fork_states.push(state);
            }

            state_at_incoming_event = match state_res::resolve(
                room_version_id,
                &fork_states,
                auth_chain_sets,
                |id| {
                    let res = db.rooms.get_pdu(id);
                    if let Err(e) = &res {
                        error!("LOOK AT ME Failed to fetch event: {}", e);
                    }
                    res.ok().flatten()
                },
            ) {
                Ok(new_state) => Some(
                    new_state
                        .into_iter()
                        .map(|((event_type, state_key), event_id)| {
                            let shortstatekey = db
                                .rooms
                                .get_or_create_shortstatekey(
                                    &event_type.to_string().into(),
                                    &state_key,
                                    &db.globals,
                                )
                                .map_err(|_| "Failed to get_or_create_shortstatekey".to_owned())?;
                            Ok((shortstatekey, event_id))
                        })
                        .collect::<Result<_, String>>()?,
                ),
                Err(e) => {
                    warn!("State resolution on prev events failed, either an event could not be found or deserialization: {}", e);
                    None
                }
            };
        }
    }

    if state_at_incoming_event.is_none() {
        warn!("Calling /state_ids");
        // Call /state_ids to find out what the state at this pdu is. We trust the server's
        // response to some extend, but we still do a lot of checks on the events
        match db
            .sending
            .send_federation_request(
                &db.globals,
                origin,
                get_room_state_ids::v1::Request {
                    room_id,
                    event_id: &incoming_pdu.event_id,
                },
            )
            .await
        {
            Ok(res) => {
                warn!("Fetching state events at event.");
                let state_vec = fetch_and_handle_outliers(
                    db,
                    origin,
                    &res.pdu_ids
                        .iter()
                        .map(|x| Arc::from(&**x))
                        .collect::<Vec<_>>(),
                    create_event,
                    room_id,
                    pub_key_map,
                )
                .await;

                let mut state: BTreeMap<_, Arc<EventId>> = BTreeMap::new();
                for (pdu, _) in state_vec {
                    let state_key = pdu
                        .state_key
                        .clone()
                        .ok_or_else(|| "Found non-state pdu in state events.".to_owned())?;

                    let shortstatekey = db
                        .rooms
                        .get_or_create_shortstatekey(
                            &pdu.kind.to_string().into(),
                            &state_key,
                            &db.globals,
                        )
                        .map_err(|_| "Failed to create shortstatekey.".to_owned())?;

                    match state.entry(shortstatekey) {
                        btree_map::Entry::Vacant(v) => {
                            v.insert(Arc::from(&*pdu.event_id));
                        }
                        btree_map::Entry::Occupied(_) => return Err(
                            "State event's type and state_key combination exists multiple times."
                                .to_owned(),
                        ),
                    }
                }

                // The original create event must still be in the state
                let create_shortstatekey = db
                    .rooms
                    .get_shortstatekey(&StateEventType::RoomCreate, "")
                    .map_err(|_| "Failed to talk to db.")?
                    .expect("Room exists");

                if state.get(&create_shortstatekey).map(|id| id.as_ref())
                    != Some(&create_event.event_id)
                {
                    return Err("Incoming event refers to wrong create event.".to_owned());
                }

                state_at_incoming_event = Some(state);
            }
            Err(e) => {
                warn!("Fetching state for event failed: {}", e);
                return Err("Fetching state for event failed".into());
            }
        };
    }

    let state_at_incoming_event =
        state_at_incoming_event.expect("we always set this to some above");

    // 11. Check the auth of the event passes based on the state of the event
    let check_result = state_res::event_auth::auth_check(
        &room_version,
        &incoming_pdu,
        None::<PduEvent>, // TODO: third party invite
        |k, s| {
            db.rooms
                .get_shortstatekey(k, s)
                .ok()
                .flatten()
                .and_then(|shortstatekey| state_at_incoming_event.get(&shortstatekey))
                .and_then(|event_id| db.rooms.get_pdu(event_id).ok().flatten())
        },
    )
    .map_err(|_e| "Auth check failed.".to_owned())?;

    if !check_result {
        return Err("Event has failed auth check with state at the event.".into());
    }
    debug!("Auth check succeeded.");

    // We start looking at current room state now, so lets lock the room

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(room_id.to_owned())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    // Now we calculate the set of extremities this room has after the incoming event has been
    // applied. We start with the previous extremities (aka leaves)
    let mut extremities = db
        .rooms
        .get_pdu_leaves(room_id)
        .map_err(|_| "Failed to load room leaves".to_owned())?;

    // Remove any forward extremities that are referenced by this incoming event's prev_events
    for prev_event in &incoming_pdu.prev_events {
        if extremities.contains(prev_event) {
            extremities.remove(prev_event);
        }
    }

    // Only keep those extremities were not referenced yet
    extremities.retain(|id| !matches!(db.rooms.is_event_referenced(room_id, id), Ok(true)));

    let current_sstatehash = db
        .rooms
        .current_shortstatehash(room_id)
        .map_err(|_| "Failed to load current state hash.".to_owned())?
        .expect("every room has state");

    let current_state_ids = db
        .rooms
        .state_full_ids(current_sstatehash)
        .map_err(|_| "Failed to load room state.")?;

    let auth_events = db
        .rooms
        .get_auth_events(
            room_id,
            &incoming_pdu.kind,
            &incoming_pdu.sender,
            incoming_pdu.state_key.as_deref(),
            &incoming_pdu.content,
        )
        .map_err(|_| "Failed to get_auth_events.".to_owned())?;

    let state_ids_compressed = state_at_incoming_event
        .iter()
        .map(|(shortstatekey, id)| {
            db.rooms
                .compress_state_event(*shortstatekey, id, &db.globals)
                .map_err(|_| "Failed to compress_state_event".to_owned())
        })
        .collect::<Result<_, _>>()?;

    // 13. Check if the event passes auth based on the "current state" of the room, if not "soft fail" it
    debug!("starting soft fail auth check");

    let soft_fail = !state_res::event_auth::auth_check(
        &room_version,
        &incoming_pdu,
        None::<PduEvent>,
        |k, s| auth_events.get(&(k.clone(), s.to_owned())),
    )
    .map_err(|_e| "Auth check failed.".to_owned())?;

    if soft_fail {
        append_incoming_pdu(
            db,
            &incoming_pdu,
            val,
            extremities.iter().map(Deref::deref),
            state_ids_compressed,
            soft_fail,
            &state_lock,
        )
        .map_err(|e| {
            warn!("Failed to add pdu to db: {}", e);
            "Failed to add pdu to db.".to_owned()
        })?;

        // Soft fail, we keep the event as an outlier but don't add it to the timeline
        warn!("Event was soft failed: {:?}", incoming_pdu);
        db.rooms
            .mark_event_soft_failed(&incoming_pdu.event_id)
            .map_err(|_| "Failed to set soft failed flag".to_owned())?;
        return Err("Event has been soft failed".into());
    }

    if incoming_pdu.state_key.is_some() {
        let mut extremity_sstatehashes = HashMap::new();

        for id in dbg!(&extremities) {
            match db
                .rooms
                .get_pdu(id)
                .map_err(|_| "Failed to ask db for pdu.".to_owned())?
            {
                Some(leaf_pdu) => {
                    extremity_sstatehashes.insert(
                        db.rooms
                            .pdu_shortstatehash(&leaf_pdu.event_id)
                            .map_err(|_| "Failed to ask db for pdu state hash.".to_owned())?
                            .ok_or_else(|| {
                                error!(
                                    "Found extremity pdu with no statehash in db: {:?}",
                                    leaf_pdu
                                );
                                "Found pdu with no statehash in db.".to_owned()
                            })?,
                        leaf_pdu,
                    );
                }
                _ => {
                    error!("Missing state snapshot for {:?}", id);
                    return Err("Missing state snapshot.".to_owned());
                }
            }
        }

        let mut fork_states = Vec::new();

        // 12. Ensure that the state is derived from the previous current state (i.e. we calculated
        //     by doing state res where one of the inputs was a previously trusted set of state,
        //     don't just trust a set of state we got from a remote).

        // We do this by adding the current state to the list of fork states
        extremity_sstatehashes.remove(&current_sstatehash);
        fork_states.push(current_state_ids);

        // We also add state after incoming event to the fork states
        let mut state_after = state_at_incoming_event.clone();
        if let Some(state_key) = &incoming_pdu.state_key {
            let shortstatekey = db
                .rooms
                .get_or_create_shortstatekey(
                    &incoming_pdu.kind.to_string().into(),
                    state_key,
                    &db.globals,
                )
                .map_err(|_| "Failed to create shortstatekey.".to_owned())?;

            state_after.insert(shortstatekey, Arc::from(&*incoming_pdu.event_id));
        }
        fork_states.push(state_after);

        let mut update_state = false;
        // 14. Use state resolution to find new room state
        let new_room_state = if fork_states.is_empty() {
            return Err("State is empty.".to_owned());
        } else if fork_states.iter().skip(1).all(|f| &fork_states[0] == f) {
            // There was only one state, so it has to be the room's current state (because that is
            // always included)
            fork_states[0]
                .iter()
                .map(|(k, id)| {
                    db.rooms
                        .compress_state_event(*k, id, &db.globals)
                        .map_err(|_| "Failed to compress_state_event.".to_owned())
                })
                .collect::<Result<_, _>>()?
        } else {
            // We do need to force an update to this room's state
            update_state = true;

            let mut auth_chain_sets = Vec::new();
            for state in &fork_states {
                auth_chain_sets.push(
                    get_auth_chain(
                        room_id,
                        state.iter().map(|(_, id)| id.clone()).collect(),
                        db,
                    )
                    .map_err(|_| "Failed to load auth chain.".to_owned())?
                    .collect(),
                );
            }

            let fork_states: Vec<_> = fork_states
                .into_iter()
                .map(|map| {
                    map.into_iter()
                        .filter_map(|(k, id)| {
                            db.rooms
                                .get_statekey_from_short(k)
                                .map(|(ty, st_key)| ((ty, st_key), id))
                                .map_err(|e| warn!("Failed to get_statekey_from_short: {}", e))
                                .ok()
                        })
                        .collect::<StateMap<_>>()
                })
                .collect();

            let state = match state_res::resolve(
                room_version_id,
                &fork_states,
                auth_chain_sets,
                |id| {
                    let res = db.rooms.get_pdu(id);
                    if let Err(e) = &res {
                        error!("LOOK AT ME Failed to fetch event: {}", e);
                    }
                    res.ok().flatten()
                },
            ) {
                Ok(new_state) => new_state,
                Err(_) => {
                    return Err("State resolution failed, either an event could not be found or deserialization".into());
                }
            };

            state
                .into_iter()
                .map(|((event_type, state_key), event_id)| {
                    let shortstatekey = db
                        .rooms
                        .get_or_create_shortstatekey(
                            &event_type.to_string().into(),
                            &state_key,
                            &db.globals,
                        )
                        .map_err(|_| "Failed to get_or_create_shortstatekey".to_owned())?;
                    db.rooms
                        .compress_state_event(shortstatekey, &event_id, &db.globals)
                        .map_err(|_| "Failed to compress state event".to_owned())
                })
                .collect::<Result<_, _>>()?
        };

        // Set the new room state to the resolved state
        if update_state {
            db.rooms
                .force_state(room_id, new_room_state, db)
                .map_err(|_| "Failed to set new room state.".to_owned())?;
        }
        debug!("Updated resolved state");
    }

    extremities.insert(incoming_pdu.event_id.clone());

    // Now that the event has passed all auth it is added into the timeline.
    // We use the `state_at_event` instead of `state_after` so we accurately
    // represent the state for this event.

    let pdu_id = append_incoming_pdu(
        db,
        &incoming_pdu,
        val,
        extremities.iter().map(Deref::deref),
        state_ids_compressed,
        soft_fail,
        &state_lock,
    )
    .map_err(|e| {
        warn!("Failed to add pdu to db: {}", e);
        "Failed to add pdu to db.".to_owned()
    })?;

    debug!("Appended incoming pdu.");

    // Event has passed all auth/stateres checks
    drop(state_lock);
    Ok(pdu_id)
}

/// Find the event and auth it. Once the event is validated (steps 1 - 8)
/// it is appended to the outliers Tree.
///
/// Returns pdu and if we fetched it over federation the raw json.
///
/// a. Look in the main timeline (pduid_pdu tree)
/// b. Look at outlier pdu tree
/// c. Ask origin server over federation
/// d. TODO: Ask other servers over federation?
#[tracing::instrument(skip_all)]
pub(crate) fn fetch_and_handle_outliers<'a>(
    db: &'a Database,
    origin: &'a ServerName,
    events: &'a [Arc<EventId>],
    create_event: &'a PduEvent,
    room_id: &'a RoomId,
    pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
) -> AsyncRecursiveType<'a, Vec<(Arc<PduEvent>, Option<BTreeMap<String, CanonicalJsonValue>>)>> {
    Box::pin(async move {
        let back_off = |id| match db.globals.bad_event_ratelimiter.write().unwrap().entry(id) {
            hash_map::Entry::Vacant(e) => {
                e.insert((Instant::now(), 1));
            }
            hash_map::Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
        };

        let mut pdus = vec![];
        for id in events {
            if let Some((time, tries)) = db.globals.bad_event_ratelimiter.read().unwrap().get(&**id)
            {
                // Exponential backoff
                let mut min_elapsed_duration = Duration::from_secs(5 * 60) * (*tries) * (*tries);
                if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
                    min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
                }

                if time.elapsed() < min_elapsed_duration {
                    info!("Backing off from {}", id);
                    continue;
                }
            }

            // a. Look in the main timeline (pduid_pdu tree)
            // b. Look at outlier pdu tree
            // (get_pdu_json checks both)
            if let Ok(Some(local_pdu)) = db.rooms.get_pdu(id) {
                trace!("Found {} in db", id);
                pdus.push((local_pdu, None));
                continue;
            }

            // c. Ask origin server over federation
            // We also handle its auth chain here so we don't get a stack overflow in
            // handle_outlier_pdu.
            let mut todo_auth_events = vec![Arc::clone(id)];
            let mut events_in_reverse_order = Vec::new();
            let mut events_all = HashSet::new();
            while let Some(next_id) = todo_auth_events.pop() {
                if events_all.contains(&next_id) {
                    continue;
                }

                if let Ok(Some(_)) = db.rooms.get_pdu(&next_id) {
                    trace!("Found {} in db", id);
                    continue;
                }

                warn!("Fetching {} over federation.", next_id);
                match db
                    .sending
                    .send_federation_request(
                        &db.globals,
                        origin,
                        get_event::v1::Request { event_id: &next_id },
                    )
                    .await
                {
                    Ok(res) => {
                        warn!("Got {} over federation", next_id);
                        let (calculated_event_id, value) =
                            match crate::pdu::gen_event_id_canonical_json(&res.pdu, db) {
                                Ok(t) => t,
                                Err(_) => {
                                    back_off((*next_id).to_owned());
                                    continue;
                                }
                            };

                        if calculated_event_id != *next_id {
                            warn!("Server didn't return event id we requested: requested: {}, we got {}. Event: {:?}",
                                next_id, calculated_event_id, &res.pdu);
                        }

                        if let Some(auth_events) =
                            value.get("auth_events").and_then(|c| c.as_array())
                        {
                            for auth_event in auth_events {
                                if let Ok(auth_event) =
                                    serde_json::from_value(auth_event.clone().into())
                                {
                                    let a: Arc<EventId> = auth_event;
                                    todo_auth_events.push(a);
                                } else {
                                    warn!("Auth event id is not valid");
                                }
                            }
                        } else {
                            warn!("Auth event list invalid");
                        }

                        events_in_reverse_order.push((next_id.clone(), value));
                        events_all.insert(next_id);
                    }
                    Err(_) => {
                        warn!("Failed to fetch event: {}", next_id);
                        back_off((*next_id).to_owned());
                    }
                }
            }

            for (next_id, value) in events_in_reverse_order.iter().rev() {
                match handle_outlier_pdu(
                    origin,
                    create_event,
                    next_id,
                    room_id,
                    value.clone(),
                    db,
                    pub_key_map,
                )
                .await
                {
                    Ok((pdu, json)) => {
                        if next_id == id {
                            pdus.push((pdu, Some(json)));
                        }
                    }
                    Err(e) => {
                        warn!("Authentication of event {} failed: {:?}", next_id, e);
                        back_off((**next_id).to_owned());
                    }
                }
            }
        }
        pdus
    })
}

/// Search the DB for the signing keys of the given server, if we don't have them
/// fetch them from the server and save to our DB.
#[tracing::instrument(skip_all)]
pub(crate) async fn fetch_signing_keys(
    db: &Database,
    origin: &ServerName,
    signature_ids: Vec<String>,
) -> Result<BTreeMap<String, Base64>> {
    let contains_all_ids =
        |keys: &BTreeMap<String, Base64>| signature_ids.iter().all(|id| keys.contains_key(id));

    let permit = db
        .globals
        .servername_ratelimiter
        .read()
        .unwrap()
        .get(origin)
        .map(|s| Arc::clone(s).acquire_owned());

    let permit = match permit {
        Some(p) => p,
        None => {
            let mut write = db.globals.servername_ratelimiter.write().unwrap();
            let s = Arc::clone(
                write
                    .entry(origin.to_owned())
                    .or_insert_with(|| Arc::new(Semaphore::new(1))),
            );

            s.acquire_owned()
        }
    }
    .await;

    let back_off = |id| match db
        .globals
        .bad_signature_ratelimiter
        .write()
        .unwrap()
        .entry(id)
    {
        hash_map::Entry::Vacant(e) => {
            e.insert((Instant::now(), 1));
        }
        hash_map::Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
    };

    if let Some((time, tries)) = db
        .globals
        .bad_signature_ratelimiter
        .read()
        .unwrap()
        .get(&signature_ids)
    {
        // Exponential backoff
        let mut min_elapsed_duration = Duration::from_secs(30) * (*tries) * (*tries);
        if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
            min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
        }

        if time.elapsed() < min_elapsed_duration {
            debug!("Backing off from {:?}", signature_ids);
            return Err(Error::BadServerResponse("bad signature, still backing off"));
        }
    }

    trace!("Loading signing keys for {}", origin);

    let mut result: BTreeMap<_, _> = db
        .globals
        .signing_keys_for(origin)?
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.key))
        .collect();

    if contains_all_ids(&result) {
        return Ok(result);
    }

    debug!("Fetching signing keys for {} over federation", origin);

    if let Some(server_key) = db
        .sending
        .send_federation_request(&db.globals, origin, get_server_keys::v2::Request::new())
        .await
        .ok()
        .and_then(|resp| resp.server_key.deserialize().ok())
    {
        db.globals.add_signing_key(origin, server_key.clone())?;

        result.extend(
            server_key
                .verify_keys
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.key)),
        );
        result.extend(
            server_key
                .old_verify_keys
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.key)),
        );

        if contains_all_ids(&result) {
            return Ok(result);
        }
    }

    for server in db.globals.trusted_servers() {
        debug!("Asking {} for {}'s signing key", server, origin);
        if let Some(server_keys) = db
            .sending
            .send_federation_request(
                &db.globals,
                server,
                get_remote_server_keys::v2::Request::new(
                    origin,
                    MilliSecondsSinceUnixEpoch::from_system_time(
                        SystemTime::now()
                            .checked_add(Duration::from_secs(3600))
                            .expect("SystemTime to large"),
                    )
                    .expect("time is valid"),
                ),
            )
            .await
            .ok()
            .map(|resp| {
                resp.server_keys
                    .into_iter()
                    .filter_map(|e| e.deserialize().ok())
                    .collect::<Vec<_>>()
            })
        {
            trace!("Got signing keys: {:?}", server_keys);
            for k in server_keys {
                db.globals.add_signing_key(origin, k.clone())?;
                result.extend(
                    k.verify_keys
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.key)),
                );
                result.extend(
                    k.old_verify_keys
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.key)),
                );
            }

            if contains_all_ids(&result) {
                return Ok(result);
            }
        }
    }

    drop(permit);

    back_off(signature_ids);

    warn!("Failed to find public key for server: {}", origin);
    Err(Error::BadServerResponse(
        "Failed to find public key for server",
    ))
}

/// Append the incoming event setting the state snapshot to the state from the
/// server that sent the event.
#[tracing::instrument(skip_all)]
fn append_incoming_pdu<'a>(
    db: &Database,
    pdu: &PduEvent,
    pdu_json: CanonicalJsonObject,
    new_room_leaves: impl IntoIterator<Item = &'a EventId> + Clone + Debug,
    state_ids_compressed: HashSet<CompressedStateEvent>,
    soft_fail: bool,
    _mutex_lock: &MutexGuard<'_, ()>, // Take mutex guard to make sure users get the room mutex
) -> Result<Option<Vec<u8>>> {
    // We append to state before appending the pdu, so we don't have a moment in time with the
    // pdu without it's state. This is okay because append_pdu can't fail.
    db.rooms.set_event_state(
        &pdu.event_id,
        &pdu.room_id,
        state_ids_compressed,
        &db.globals,
    )?;

    if soft_fail {
        db.rooms
            .mark_as_referenced(&pdu.room_id, &pdu.prev_events)?;
        db.rooms.replace_pdu_leaves(&pdu.room_id, new_room_leaves)?;
        return Ok(None);
    }

    let pdu_id = db.rooms.append_pdu(pdu, pdu_json, new_room_leaves, db)?;

    for appservice in db.appservice.all()? {
        if db.rooms.appservice_in_room(&pdu.room_id, &appservice, db)? {
            db.sending.send_pdu_appservice(&appservice.0, &pdu_id)?;
            continue;
        }

        if let Some(namespaces) = appservice.1.get("namespaces") {
            let users = namespaces
                .get("users")
                .and_then(|users| users.as_sequence())
                .map_or_else(Vec::new, |users| {
                    users
                        .iter()
                        .filter_map(|users| Regex::new(users.get("regex")?.as_str()?).ok())
                        .collect::<Vec<_>>()
                });
            let aliases = namespaces
                .get("aliases")
                .and_then(|aliases| aliases.as_sequence())
                .map_or_else(Vec::new, |aliases| {
                    aliases
                        .iter()
                        .filter_map(|aliases| Regex::new(aliases.get("regex")?.as_str()?).ok())
                        .collect::<Vec<_>>()
                });
            let rooms = namespaces
                .get("rooms")
                .and_then(|rooms| rooms.as_sequence());

            let matching_users = |users: &Regex| {
                users.is_match(pdu.sender.as_str())
                    || pdu.kind == RoomEventType::RoomMember
                        && pdu
                            .state_key
                            .as_ref()
                            .map_or(false, |state_key| users.is_match(state_key))
            };
            let matching_aliases = |aliases: &Regex| {
                db.rooms
                    .room_aliases(&pdu.room_id)
                    .filter_map(|r| r.ok())
                    .any(|room_alias| aliases.is_match(room_alias.as_str()))
            };

            if aliases.iter().any(matching_aliases)
                || rooms.map_or(false, |rooms| rooms.contains(&pdu.room_id.as_str().into()))
                || users.iter().any(matching_users)
            {
                db.sending.send_pdu_appservice(&appservice.0, &pdu_id)?;
            }
        }
    }

    Ok(Some(pdu_id))
}

#[tracing::instrument(skip(starting_events, db))]
pub(crate) fn get_auth_chain<'a>(
    room_id: &RoomId,
    starting_events: Vec<Arc<EventId>>,
    db: &'a Database,
) -> Result<impl Iterator<Item = Arc<EventId>> + 'a> {
    const NUM_BUCKETS: usize = 50;

    let mut buckets = vec![BTreeSet::new(); NUM_BUCKETS];

    for id in starting_events {
        let short = db.rooms.get_or_create_shorteventid(&id, &db.globals)?;
        let bucket_id = (short % NUM_BUCKETS as u64) as usize;
        buckets[bucket_id].insert((short, id.clone()));
    }

    let mut full_auth_chain = HashSet::new();

    let mut hits = 0;
    let mut misses = 0;
    for chunk in buckets {
        if chunk.is_empty() {
            continue;
        }

        let chunk_key: Vec<u64> = chunk.iter().map(|(short, _)| short).copied().collect();
        if let Some(cached) = db.rooms.get_auth_chain_from_cache(&chunk_key)? {
            hits += 1;
            full_auth_chain.extend(cached.iter().copied());
            continue;
        }
        misses += 1;

        let mut chunk_cache = HashSet::new();
        let mut hits2 = 0;
        let mut misses2 = 0;
        for (sevent_id, event_id) in chunk {
            if let Some(cached) = db.rooms.get_auth_chain_from_cache(&[sevent_id])? {
                hits2 += 1;
                chunk_cache.extend(cached.iter().copied());
            } else {
                misses2 += 1;
                let auth_chain = Arc::new(get_auth_chain_inner(room_id, &event_id, db)?);
                db.rooms
                    .cache_auth_chain(vec![sevent_id], Arc::clone(&auth_chain))?;
                println!(
                    "cache missed event {} with auth chain len {}",
                    event_id,
                    auth_chain.len()
                );
                chunk_cache.extend(auth_chain.iter());
            };
        }
        println!(
            "chunk missed with len {}, event hits2: {}, misses2: {}",
            chunk_cache.len(),
            hits2,
            misses2
        );
        let chunk_cache = Arc::new(chunk_cache);
        db.rooms
            .cache_auth_chain(chunk_key, Arc::clone(&chunk_cache))?;
        full_auth_chain.extend(chunk_cache.iter());
    }

    println!(
        "total: {}, chunk hits: {}, misses: {}",
        full_auth_chain.len(),
        hits,
        misses
    );

    Ok(full_auth_chain
        .into_iter()
        .filter_map(move |sid| db.rooms.get_eventid_from_short(sid).ok()))
}

#[tracing::instrument(skip(event_id, db))]
fn get_auth_chain_inner(
    room_id: &RoomId,
    event_id: &EventId,
    db: &Database,
) -> Result<HashSet<u64>> {
    let mut todo = vec![Arc::from(event_id)];
    let mut found = HashSet::new();

    while let Some(event_id) = todo.pop() {
        match db.rooms.get_pdu(&event_id) {
            Ok(Some(pdu)) => {
                if pdu.room_id != room_id {
                    return Err(Error::BadRequest(ErrorKind::Forbidden, "Evil event in db"));
                }
                for auth_event in &pdu.auth_events {
                    let sauthevent = db
                        .rooms
                        .get_or_create_shorteventid(auth_event, &db.globals)?;

                    if !found.contains(&sauthevent) {
                        found.insert(sauthevent);
                        todo.push(auth_event.clone());
                    }
                }
            }
            Ok(None) => {
                warn!("Could not find pdu mentioned in auth events: {}", event_id);
            }
            Err(e) => {
                warn!("Could not load event in auth chain: {} {}", event_id, e);
            }
        }
    }

    Ok(found)
}

/// # `GET /_matrix/federation/v1/event/{eventId}`
///
/// Retrieves a single event from the server.
///
/// - Only works if a user of this server is currently invited or joined the room
pub async fn get_event_route(
    db: DatabaseGuard,
    body: Ruma<get_event::v1::IncomingRequest>,
) -> Result<get_event::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    let event = db
        .rooms
        .get_pdu_json(&body.event_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Event not found."))?;

    let room_id_str = event
        .get("room_id")
        .and_then(|val| val.as_str())
        .ok_or_else(|| Error::bad_database("Invalid event in database"))?;

    let room_id = <&RoomId>::try_from(room_id_str)
        .map_err(|_| Error::bad_database("Invalid room id field in event in database"))?;

    if !db.rooms.server_in_room(sender_servername, room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room",
        ));
    }

    Ok(get_event::v1::Response {
        origin: db.globals.server_name().to_owned(),
        origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
        pdu: PduEvent::convert_to_outgoing_federation_event(event),
    })
}

/// # `POST /_matrix/federation/v1/get_missing_events/{roomId}`
///
/// Retrieves events that the sender is missing.
pub async fn get_missing_events_route(
    db: DatabaseGuard,
    body: Ruma<get_missing_events::v1::IncomingRequest>,
) -> Result<get_missing_events::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    if !db.rooms.server_in_room(sender_servername, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room",
        ));
    }

    acl_check(sender_servername, &body.room_id, &db)?;

    let mut queued_events = body.latest_events.clone();
    let mut events = Vec::new();

    let mut i = 0;
    while i < queued_events.len() && events.len() < u64::from(body.limit) as usize {
        if let Some(pdu) = db.rooms.get_pdu_json(&queued_events[i])? {
            let room_id_str = pdu
                .get("room_id")
                .and_then(|val| val.as_str())
                .ok_or_else(|| Error::bad_database("Invalid event in database"))?;

            let event_room_id = <&RoomId>::try_from(room_id_str)
                .map_err(|_| Error::bad_database("Invalid room id field in event in database"))?;

            if event_room_id != body.room_id {
                warn!(
                    "Evil event detected: Event {} found while searching in room {}",
                    queued_events[i], body.room_id
                );
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Evil event detected",
                ));
            }

            if body.earliest_events.contains(&queued_events[i]) {
                i += 1;
                continue;
            }
            queued_events.extend_from_slice(
                &serde_json::from_value::<Vec<Box<EventId>>>(
                    serde_json::to_value(pdu.get("prev_events").cloned().ok_or_else(|| {
                        Error::bad_database("Event in db has no prev_events field.")
                    })?)
                    .expect("canonical json is valid json value"),
                )
                .map_err(|_| Error::bad_database("Invalid prev_events content in pdu in db."))?,
            );
            events.push(PduEvent::convert_to_outgoing_federation_event(pdu));
        }
        i += 1;
    }

    Ok(get_missing_events::v1::Response { events })
}

/// # `GET /_matrix/federation/v1/event_auth/{roomId}/{eventId}`
///
/// Retrieves the auth chain for a given event.
///
/// - This does not include the event itself
pub async fn get_event_authorization_route(
    db: DatabaseGuard,
    body: Ruma<get_event_authorization::v1::IncomingRequest>,
) -> Result<get_event_authorization::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    if !db.rooms.server_in_room(sender_servername, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room.",
        ));
    }

    acl_check(sender_servername, &body.room_id, &db)?;

    let event = db
        .rooms
        .get_pdu_json(&body.event_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Event not found."))?;

    let room_id_str = event
        .get("room_id")
        .and_then(|val| val.as_str())
        .ok_or_else(|| Error::bad_database("Invalid event in database"))?;

    let room_id = <&RoomId>::try_from(room_id_str)
        .map_err(|_| Error::bad_database("Invalid room id field in event in database"))?;

    let auth_chain_ids = get_auth_chain(room_id, vec![Arc::from(&*body.event_id)], &db)?;

    Ok(get_event_authorization::v1::Response {
        auth_chain: auth_chain_ids
            .filter_map(|id| db.rooms.get_pdu_json(&id).ok()?)
            .map(PduEvent::convert_to_outgoing_federation_event)
            .collect(),
    })
}

/// # `GET /_matrix/federation/v1/state/{roomId}`
///
/// Retrieves the current state of the room.
pub async fn get_room_state_route(
    db: DatabaseGuard,
    body: Ruma<get_room_state::v1::IncomingRequest>,
) -> Result<get_room_state::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    if !db.rooms.server_in_room(sender_servername, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room.",
        ));
    }

    acl_check(sender_servername, &body.room_id, &db)?;

    let shortstatehash = db
        .rooms
        .pdu_shortstatehash(&body.event_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Pdu state not found.",
        ))?;

    let pdus = db
        .rooms
        .state_full_ids(shortstatehash)?
        .into_iter()
        .map(|(_, id)| {
            PduEvent::convert_to_outgoing_federation_event(
                db.rooms.get_pdu_json(&id).unwrap().unwrap(),
            )
        })
        .collect();

    let auth_chain_ids = get_auth_chain(&body.room_id, vec![Arc::from(&*body.event_id)], &db)?;

    Ok(get_room_state::v1::Response {
        auth_chain: auth_chain_ids
            .map(|id| {
                db.rooms.get_pdu_json(&id).map(|maybe_json| {
                    PduEvent::convert_to_outgoing_federation_event(maybe_json.unwrap())
                })
            })
            .filter_map(|r| r.ok())
            .collect(),
        pdus,
    })
}

/// # `GET /_matrix/federation/v1/state_ids/{roomId}`
///
/// Retrieves the current state of the room.
pub async fn get_room_state_ids_route(
    db: DatabaseGuard,
    body: Ruma<get_room_state_ids::v1::IncomingRequest>,
) -> Result<get_room_state_ids::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    if !db.rooms.server_in_room(sender_servername, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room.",
        ));
    }

    acl_check(sender_servername, &body.room_id, &db)?;

    let shortstatehash = db
        .rooms
        .pdu_shortstatehash(&body.event_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Pdu state not found.",
        ))?;

    let pdu_ids = db
        .rooms
        .state_full_ids(shortstatehash)?
        .into_iter()
        .map(|(_, id)| (*id).to_owned())
        .collect();

    let auth_chain_ids = get_auth_chain(&body.room_id, vec![Arc::from(&*body.event_id)], &db)?;

    Ok(get_room_state_ids::v1::Response {
        auth_chain_ids: auth_chain_ids.map(|id| (*id).to_owned()).collect(),
        pdu_ids,
    })
}

/// # `GET /_matrix/federation/v1/make_join/{roomId}/{userId}`
///
/// Creates a join template.
pub async fn create_join_event_template_route(
    db: DatabaseGuard,
    body: Ruma<prepare_join_event::v1::IncomingRequest>,
) -> Result<prepare_join_event::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    if !db.rooms.exists(&body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Room is unknown to this server.",
        ));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    acl_check(sender_servername, &body.room_id, &db)?;

    // TODO: Conduit does not implement restricted join rules yet, we always reject
    let join_rules_event =
        db.rooms
            .room_state_get(&body.room_id, &StateEventType::RoomJoinRules, "")?;

    let join_rules_event_content: Option<RoomJoinRulesEventContent> = join_rules_event
        .as_ref()
        .map(|join_rules_event| {
            serde_json::from_str(join_rules_event.content.get()).map_err(|e| {
                warn!("Invalid join rules event: {}", e);
                Error::bad_database("Invalid join rules event in db.")
            })
        })
        .transpose()?;

    if let Some(join_rules_event_content) = join_rules_event_content {
        if matches!(
            join_rules_event_content.join_rule,
            JoinRule::Restricted { .. }
        ) {
            return Err(Error::BadRequest(
                ErrorKind::Unknown,
                "Conduit does not support restricted rooms yet.",
            ));
        }
    }

    let prev_events: Vec<_> = db
        .rooms
        .get_pdu_leaves(&body.room_id)?
        .into_iter()
        .take(20)
        .collect();

    let create_event = db
        .rooms
        .room_state_get(&body.room_id, &StateEventType::RoomCreate, "")?;

    let create_event_content: Option<RoomCreateEventContent> = create_event
        .as_ref()
        .map(|create_event| {
            serde_json::from_str(create_event.content.get()).map_err(|e| {
                warn!("Invalid create event: {}", e);
                Error::bad_database("Invalid create event in db.")
            })
        })
        .transpose()?;

    // If there was no create event yet, assume we are creating a room with the default version
    // right now
    let room_version_id = create_event_content
        .map_or(db.globals.default_room_version(), |create_event| {
            create_event.room_version
        });
    let room_version = RoomVersion::new(&room_version_id).expect("room version is supported");

    if !body.ver.contains(&room_version_id) {
        return Err(Error::BadRequest(
            ErrorKind::IncompatibleRoomVersion {
                room_version: room_version_id,
            },
            "Room version not supported.",
        ));
    }

    let content = to_raw_value(&RoomMemberEventContent {
        avatar_url: None,
        blurhash: None,
        displayname: None,
        is_direct: None,
        membership: MembershipState::Join,
        third_party_invite: None,
        reason: None,
        join_authorized_via_users_server: None,
    })
    .expect("member event is valid value");

    let state_key = body.user_id.to_string();
    let kind = StateEventType::RoomMember;

    let auth_events = db.rooms.get_auth_events(
        &body.room_id,
        &kind.to_string().into(),
        &body.user_id,
        Some(&state_key),
        &content,
    )?;

    // Our depth is the maximum depth of prev_events + 1
    let depth = prev_events
        .iter()
        .filter_map(|event_id| Some(db.rooms.get_pdu(event_id).ok()??.depth))
        .max()
        .unwrap_or_else(|| uint!(0))
        + uint!(1);

    let mut unsigned = BTreeMap::new();

    if let Some(prev_pdu) = db.rooms.room_state_get(&body.room_id, &kind, &state_key)? {
        unsigned.insert("prev_content".to_owned(), prev_pdu.content.clone());
        unsigned.insert(
            "prev_sender".to_owned(),
            to_raw_value(&prev_pdu.sender).expect("UserId is valid"),
        );
    }

    let pdu = PduEvent {
        event_id: ruma::event_id!("$thiswillbefilledinlater").into(),
        room_id: body.room_id.clone(),
        sender: body.user_id.clone(),
        origin_server_ts: utils::millis_since_unix_epoch()
            .try_into()
            .expect("time is valid"),
        kind: kind.to_string().into(),
        content,
        state_key: Some(state_key),
        prev_events,
        depth,
        auth_events: auth_events
            .iter()
            .map(|(_, pdu)| pdu.event_id.clone())
            .collect(),
        redacts: None,
        unsigned: if unsigned.is_empty() {
            None
        } else {
            Some(to_raw_value(&unsigned).expect("to_raw_value always works"))
        },
        hashes: EventHash {
            sha256: "aaa".to_owned(),
        },
        signatures: None,
    };

    let auth_check = state_res::auth_check(
        &room_version,
        &pdu,
        None::<PduEvent>, // TODO: third_party_invite
        |k, s| auth_events.get(&(k.clone(), s.to_owned())),
    )
    .map_err(|e| {
        error!("{:?}", e);
        Error::bad_database("Auth check failed.")
    })?;

    if !auth_check {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Event is not authorized.",
        ));
    }

    // Hash and sign
    let mut pdu_json =
        utils::to_canonical_object(&pdu).expect("event is valid, we just created it");

    pdu_json.remove("event_id");

    // Add origin because synapse likes that (and it's required in the spec)
    pdu_json.insert(
        "origin".to_owned(),
        CanonicalJsonValue::String(db.globals.server_name().as_str().to_owned()),
    );

    Ok(prepare_join_event::v1::Response {
        room_version: Some(room_version_id),
        event: to_raw_value(&pdu_json).expect("CanonicalJson can be serialized to JSON"),
    })
}

async fn create_join_event(
    db: &DatabaseGuard,
    sender_servername: &ServerName,
    room_id: &RoomId,
    pdu: &RawJsonValue,
) -> Result<RoomState> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    if !db.rooms.exists(room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Room is unknown to this server.",
        ));
    }

    acl_check(sender_servername, room_id, db)?;

    // TODO: Conduit does not implement restricted join rules yet, we always reject
    let join_rules_event = db
        .rooms
        .room_state_get(room_id, &StateEventType::RoomJoinRules, "")?;

    let join_rules_event_content: Option<RoomJoinRulesEventContent> = join_rules_event
        .as_ref()
        .map(|join_rules_event| {
            serde_json::from_str(join_rules_event.content.get()).map_err(|e| {
                warn!("Invalid join rules event: {}", e);
                Error::bad_database("Invalid join rules event in db.")
            })
        })
        .transpose()?;

    if let Some(join_rules_event_content) = join_rules_event_content {
        if matches!(
            join_rules_event_content.join_rule,
            JoinRule::Restricted { .. }
        ) {
            return Err(Error::BadRequest(
                ErrorKind::Unknown,
                "Conduit does not support restricted rooms yet.",
            ));
        }
    }

    // We need to return the state prior to joining, let's keep a reference to that here
    let shortstatehash = db
        .rooms
        .current_shortstatehash(room_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Pdu state not found.",
        ))?;

    let pub_key_map = RwLock::new(BTreeMap::new());
    // let mut auth_cache = EventMap::new();

    // We do not add the event_id field to the pdu here because of signature and hashes checks
    let (event_id, value) = match crate::pdu::gen_event_id_canonical_json(pdu, db) {
        Ok(t) => t,
        Err(_) => {
            // Event could not be converted to canonical json
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Could not convert event to canonical json.",
            ));
        }
    };

    let origin: Box<ServerName> = serde_json::from_value(
        serde_json::to_value(value.get("origin").ok_or(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Event needs an origin field.",
        ))?)
        .expect("CanonicalJson is valid json value"),
    )
    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Origin field is invalid."))?;

    let mutex = Arc::clone(
        db.globals
            .roomid_mutex_federation
            .write()
            .unwrap()
            .entry(room_id.to_owned())
            .or_default(),
    );
    let mutex_lock = mutex.lock().await;
    let pdu_id = handle_incoming_pdu(&origin, &event_id, room_id, value, true, db, &pub_key_map)
        .await
        .map_err(|e| {
            warn!("Error while handling incoming send join PDU: {}", e);
            Error::BadRequest(
                ErrorKind::InvalidParam,
                "Error while handling incoming PDU.",
            )
        })?
        .ok_or(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Could not accept incoming PDU as timeline event.",
        ))?;
    drop(mutex_lock);

    let state_ids = db.rooms.state_full_ids(shortstatehash)?;
    let auth_chain_ids = get_auth_chain(
        room_id,
        state_ids.iter().map(|(_, id)| id.clone()).collect(),
        db,
    )?;

    let servers = db
        .rooms
        .room_servers(room_id)
        .filter_map(|r| r.ok())
        .filter(|server| &**server != db.globals.server_name());

    db.sending.send_pdu(servers, &pdu_id)?;

    db.flush()?;

    Ok(RoomState {
        auth_chain: auth_chain_ids
            .filter_map(|id| db.rooms.get_pdu_json(&id).ok().flatten())
            .map(PduEvent::convert_to_outgoing_federation_event)
            .collect(),
        state: state_ids
            .iter()
            .filter_map(|(_, id)| db.rooms.get_pdu_json(id).ok().flatten())
            .map(PduEvent::convert_to_outgoing_federation_event)
            .collect(),
    })
}

/// # `PUT /_matrix/federation/v1/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub async fn create_join_event_v1_route(
    db: DatabaseGuard,
    body: Ruma<create_join_event::v1::IncomingRequest>,
) -> Result<create_join_event::v1::Response> {
    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    let room_state = create_join_event(&db, sender_servername, &body.room_id, &body.pdu).await?;

    Ok(create_join_event::v1::Response { room_state })
}

/// # `PUT /_matrix/federation/v2/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub async fn create_join_event_v2_route(
    db: DatabaseGuard,
    body: Ruma<create_join_event::v2::IncomingRequest>,
) -> Result<create_join_event::v2::Response> {
    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    let room_state = create_join_event(&db, sender_servername, &body.room_id, &body.pdu).await?;

    Ok(create_join_event::v2::Response { room_state })
}

/// # `PUT /_matrix/federation/v2/invite/{roomId}/{eventId}`
///
/// Invites a remote user to a room.
pub async fn create_invite_route(
    db: DatabaseGuard,
    body: Ruma<create_invite::v2::IncomingRequest>,
) -> Result<create_invite::v2::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    acl_check(sender_servername, &body.room_id, &db)?;

    if !db.rooms.is_supported_version(&db, &body.room_version) {
        return Err(Error::BadRequest(
            ErrorKind::IncompatibleRoomVersion {
                room_version: body.room_version.clone(),
            },
            "Server does not support this room version.",
        ));
    }

    let mut signed_event = utils::to_canonical_object(&body.event)
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invite event is invalid."))?;

    ruma::signatures::hash_and_sign_event(
        db.globals.server_name().as_str(),
        db.globals.keypair(),
        &mut signed_event,
        &body.room_version,
    )
    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Failed to sign event."))?;

    // Generate event id
    let event_id = EventId::parse(format!(
        "${}",
        ruma::signatures::reference_hash(&signed_event, &body.room_version)
            .expect("ruma can calculate reference hashes")
    ))
    .expect("ruma's reference hashes are valid event ids");

    // Add event_id back
    signed_event.insert(
        "event_id".to_owned(),
        CanonicalJsonValue::String(event_id.into()),
    );

    let sender: Box<_> = serde_json::from_value(
        signed_event
            .get("sender")
            .ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event had no sender field.",
            ))?
            .clone()
            .into(),
    )
    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "sender is not a user id."))?;

    let invited_user: Box<_> = serde_json::from_value(
        signed_event
            .get("state_key")
            .ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event had no state_key field.",
            ))?
            .clone()
            .into(),
    )
    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "state_key is not a user id."))?;

    let mut invite_state = body.invite_room_state.clone();

    let mut event: JsonObject = serde_json::from_str(body.event.get())
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid invite event bytes."))?;

    event.insert("event_id".to_owned(), "$dummy".into());

    let pdu: PduEvent = serde_json::from_value(event.into()).map_err(|e| {
        warn!("Invalid invite event: {}", e);
        Error::BadRequest(ErrorKind::InvalidParam, "Invalid invite event.")
    })?;

    invite_state.push(pdu.to_stripped_state_event());

    // If the room already exists, the remote server will notify us about the join via /send
    if !db.rooms.exists(&pdu.room_id)? {
        db.rooms.update_membership(
            &body.room_id,
            &invited_user,
            MembershipState::Invite,
            &sender,
            Some(invite_state),
            &db,
            true,
        )?;
    }

    db.flush()?;

    Ok(create_invite::v2::Response {
        event: PduEvent::convert_to_outgoing_federation_event(signed_event),
    })
}

/// # `GET /_matrix/federation/v1/user/devices/{userId}`
///
/// Gets information on all devices of the user.
pub async fn get_devices_route(
    db: DatabaseGuard,
    body: Ruma<get_devices::v1::IncomingRequest>,
) -> Result<get_devices::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    Ok(get_devices::v1::Response {
        user_id: body.user_id.clone(),
        stream_id: db
            .users
            .get_devicelist_version(&body.user_id)?
            .unwrap_or(0)
            .try_into()
            .expect("version will not grow that large"),
        devices: db
            .users
            .all_devices_metadata(&body.user_id)
            .filter_map(|r| r.ok())
            .filter_map(|metadata| {
                Some(UserDevice {
                    keys: db
                        .users
                        .get_device_keys(&body.user_id, &metadata.device_id)
                        .ok()??,
                    device_id: metadata.device_id,
                    device_display_name: metadata.display_name,
                })
            })
            .collect(),
        master_key: db
            .users
            .get_master_key(&body.user_id, |u| u.server_name() == sender_servername)?,
        self_signing_key: db
            .users
            .get_self_signing_key(&body.user_id, |u| u.server_name() == sender_servername)?,
    })
}

/// # `GET /_matrix/federation/v1/query/directory`
///
/// Resolve a room alias to a room id.
pub async fn get_room_information_route(
    db: DatabaseGuard,
    body: Ruma<get_room_information::v1::IncomingRequest>,
) -> Result<get_room_information::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let room_id = db
        .rooms
        .id_from_alias(&body.room_alias)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Room alias not found.",
        ))?;

    Ok(get_room_information::v1::Response {
        room_id,
        servers: vec![db.globals.server_name().to_owned()],
    })
}

/// # `GET /_matrix/federation/v1/query/profile`
///
/// Gets information on a profile.
pub async fn get_profile_information_route(
    db: DatabaseGuard,
    body: Ruma<get_profile_information::v1::IncomingRequest>,
) -> Result<get_profile_information::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut displayname = None;
    let mut avatar_url = None;
    let mut blurhash = None;

    match &body.field {
        Some(ProfileField::DisplayName) => displayname = db.users.displayname(&body.user_id)?,
        Some(ProfileField::AvatarUrl) => {
            avatar_url = db.users.avatar_url(&body.user_id)?;
            blurhash = db.users.blurhash(&body.user_id)?
        }
        // TODO: what to do with custom
        Some(_) => {}
        None => {
            displayname = db.users.displayname(&body.user_id)?;
            avatar_url = db.users.avatar_url(&body.user_id)?;
            blurhash = db.users.blurhash(&body.user_id)?;
        }
    }

    Ok(get_profile_information::v1::Response {
        blurhash,
        displayname,
        avatar_url,
    })
}

/// # `POST /_matrix/federation/v1/user/keys/query`
///
/// Gets devices and identity keys for the given users.
pub async fn get_keys_route(
    db: DatabaseGuard,
    body: Ruma<get_keys::v1::Request>,
) -> Result<get_keys::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let result = get_keys_helper(
        None,
        &body.device_keys,
        |u| Some(u.server_name()) == body.sender_servername.as_deref(),
        &db,
    )
    .await?;

    db.flush()?;

    Ok(get_keys::v1::Response {
        device_keys: result.device_keys,
        master_keys: result.master_keys,
        self_signing_keys: result.self_signing_keys,
    })
}

/// # `POST /_matrix/federation/v1/user/keys/claim`
///
/// Claims one-time keys.
pub async fn claim_keys_route(
    db: DatabaseGuard,
    body: Ruma<claim_keys::v1::Request>,
) -> Result<claim_keys::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let result = claim_keys_helper(&body.one_time_keys, &db).await?;

    db.flush()?;

    Ok(claim_keys::v1::Response {
        one_time_keys: result.one_time_keys,
    })
}

#[tracing::instrument(skip_all)]
pub(crate) async fn fetch_required_signing_keys(
    event: &BTreeMap<String, CanonicalJsonValue>,
    pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    db: &Database,
) -> Result<()> {
    let signatures = event
        .get("signatures")
        .ok_or(Error::BadServerResponse(
            "No signatures in server response pdu.",
        ))?
        .as_object()
        .ok_or(Error::BadServerResponse(
            "Invalid signatures object in server response pdu.",
        ))?;

    // We go through all the signatures we see on the value and fetch the corresponding signing
    // keys
    for (signature_server, signature) in signatures {
        let signature_object = signature.as_object().ok_or(Error::BadServerResponse(
            "Invalid signatures content object in server response pdu.",
        ))?;

        let signature_ids = signature_object.keys().cloned().collect::<Vec<_>>();

        let fetch_res = fetch_signing_keys(
            db,
            signature_server.as_str().try_into().map_err(|_| {
                Error::BadServerResponse("Invalid servername in signatures of server response pdu.")
            })?,
            signature_ids,
        )
        .await;

        let keys = match fetch_res {
            Ok(keys) => keys,
            Err(_) => {
                warn!("Signature verification failed: Could not fetch signing key.",);
                continue;
            }
        };

        pub_key_map
            .write()
            .map_err(|_| Error::bad_database("RwLock is poisoned."))?
            .insert(signature_server.clone(), keys);
    }

    Ok(())
}

// Gets a list of servers for which we don't have the signing key yet. We go over
// the PDUs and either cache the key or add it to the list that needs to be retrieved.
fn get_server_keys_from_cache(
    pdu: &RawJsonValue,
    servers: &mut BTreeMap<Box<ServerName>, BTreeMap<Box<ServerSigningKeyId>, QueryCriteria>>,
    room_version: &RoomVersionId,
    pub_key_map: &mut RwLockWriteGuard<'_, BTreeMap<String, BTreeMap<String, Base64>>>,
    db: &Database,
) -> Result<()> {
    let value: CanonicalJsonObject = serde_json::from_str(pdu.get()).map_err(|e| {
        error!("Invalid PDU in server response: {:?}: {:?}", pdu, e);
        Error::BadServerResponse("Invalid PDU in server response")
    })?;

    let event_id = format!(
        "${}",
        ruma::signatures::reference_hash(&value, room_version)
            .expect("ruma can calculate reference hashes")
    );
    let event_id = <&EventId>::try_from(event_id.as_str())
        .expect("ruma's reference hashes are valid event ids");

    if let Some((time, tries)) = db
        .globals
        .bad_event_ratelimiter
        .read()
        .unwrap()
        .get(event_id)
    {
        // Exponential backoff
        let mut min_elapsed_duration = Duration::from_secs(30) * (*tries) * (*tries);
        if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
            min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
        }

        if time.elapsed() < min_elapsed_duration {
            debug!("Backing off from {}", event_id);
            return Err(Error::BadServerResponse("bad event, still backing off"));
        }
    }

    let signatures = value
        .get("signatures")
        .ok_or(Error::BadServerResponse(
            "No signatures in server response pdu.",
        ))?
        .as_object()
        .ok_or(Error::BadServerResponse(
            "Invalid signatures object in server response pdu.",
        ))?;

    for (signature_server, signature) in signatures {
        let signature_object = signature.as_object().ok_or(Error::BadServerResponse(
            "Invalid signatures content object in server response pdu.",
        ))?;

        let signature_ids = signature_object.keys().cloned().collect::<Vec<_>>();

        let contains_all_ids =
            |keys: &BTreeMap<String, Base64>| signature_ids.iter().all(|id| keys.contains_key(id));

        let origin = <&ServerName>::try_from(signature_server.as_str()).map_err(|_| {
            Error::BadServerResponse("Invalid servername in signatures of server response pdu.")
        })?;

        if servers.contains_key(origin) || pub_key_map.contains_key(origin.as_str()) {
            continue;
        }

        trace!("Loading signing keys for {}", origin);

        let result: BTreeMap<_, _> = db
            .globals
            .signing_keys_for(origin)?
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.key))
            .collect();

        if !contains_all_ids(&result) {
            trace!("Signing key not loaded for {}", origin);
            servers.insert(origin.to_owned(), BTreeMap::new());
        }

        pub_key_map.insert(origin.to_string(), result);
    }

    Ok(())
}

pub(crate) async fn fetch_join_signing_keys(
    event: &create_join_event::v2::Response,
    room_version: &RoomVersionId,
    pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    db: &Database,
) -> Result<()> {
    let mut servers: BTreeMap<Box<ServerName>, BTreeMap<Box<ServerSigningKeyId>, QueryCriteria>> =
        BTreeMap::new();

    {
        let mut pkm = pub_key_map
            .write()
            .map_err(|_| Error::bad_database("RwLock is poisoned."))?;

        // Try to fetch keys, failure is okay
        // Servers we couldn't find in the cache will be added to `servers`
        for pdu in &event.room_state.state {
            let _ = get_server_keys_from_cache(pdu, &mut servers, room_version, &mut pkm, db);
        }
        for pdu in &event.room_state.auth_chain {
            let _ = get_server_keys_from_cache(pdu, &mut servers, room_version, &mut pkm, db);
        }

        drop(pkm);
    }

    if servers.is_empty() {
        // We had all keys locally
        return Ok(());
    }

    for server in db.globals.trusted_servers() {
        trace!("Asking batch signing keys from trusted server {}", server);
        if let Ok(keys) = db
            .sending
            .send_federation_request(
                &db.globals,
                server,
                get_remote_server_keys_batch::v2::Request {
                    server_keys: servers.clone(),
                },
            )
            .await
        {
            trace!("Got signing keys: {:?}", keys);
            let mut pkm = pub_key_map
                .write()
                .map_err(|_| Error::bad_database("RwLock is poisoned."))?;
            for k in keys.server_keys {
                let k = k.deserialize().unwrap();

                // TODO: Check signature from trusted server?
                servers.remove(&k.server_name);

                let result = db
                    .globals
                    .add_signing_key(&k.server_name, k.clone())?
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v.key))
                    .collect::<BTreeMap<_, _>>();

                pkm.insert(k.server_name.to_string(), result);
            }
        }

        if servers.is_empty() {
            return Ok(());
        }
    }

    let mut futures: FuturesUnordered<_> = servers
        .into_iter()
        .map(|(server, _)| async move {
            (
                db.sending
                    .send_federation_request(
                        &db.globals,
                        &server,
                        get_server_keys::v2::Request::new(),
                    )
                    .await,
                server,
            )
        })
        .collect();

    while let Some(result) = futures.next().await {
        if let (Ok(get_keys_response), origin) = result {
            let result: BTreeMap<_, _> = db
                .globals
                .add_signing_key(&origin, get_keys_response.server_key.deserialize().unwrap())?
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.key))
                .collect();

            pub_key_map
                .write()
                .map_err(|_| Error::bad_database("RwLock is poisoned."))?
                .insert(origin.to_string(), result);
        }
    }

    Ok(())
}

/// Returns Ok if the acl allows the server
fn acl_check(server_name: &ServerName, room_id: &RoomId, db: &Database) -> Result<()> {
    let acl_event = match db
        .rooms
        .room_state_get(room_id, &StateEventType::RoomServerAcl, "")?
    {
        Some(acl) => acl,
        None => return Ok(()),
    };

    let acl_event_content: RoomServerAclEventContent =
        match serde_json::from_str(acl_event.content.get()) {
            Ok(content) => content,
            Err(_) => {
                warn!("Invalid ACL event");
                return Ok(());
            }
        };

    if acl_event_content.is_allowed(server_name) {
        Ok(())
    } else {
        Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server was denied by ACL",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{add_port_to_hostname, get_ip_with_port, FedDest};

    #[test]
    fn ips_get_default_ports() {
        assert_eq!(
            get_ip_with_port("1.1.1.1"),
            Some(FedDest::Literal("1.1.1.1:8448".parse().unwrap()))
        );
        assert_eq!(
            get_ip_with_port("dead:beef::"),
            Some(FedDest::Literal("[dead:beef::]:8448".parse().unwrap()))
        );
    }

    #[test]
    fn ips_keep_custom_ports() {
        assert_eq!(
            get_ip_with_port("1.1.1.1:1234"),
            Some(FedDest::Literal("1.1.1.1:1234".parse().unwrap()))
        );
        assert_eq!(
            get_ip_with_port("[dead::beef]:8933"),
            Some(FedDest::Literal("[dead::beef]:8933".parse().unwrap()))
        );
    }

    #[test]
    fn hostnames_get_default_ports() {
        assert_eq!(
            add_port_to_hostname("example.com"),
            FedDest::Named(String::from("example.com"), String::from(":8448"))
        )
    }

    #[test]
    fn hostnames_keep_custom_ports() {
        assert_eq!(
            add_port_to_hostname("example.com:1337"),
            FedDest::Named(String::from("example.com"), String::from(":1337"))
        )
    }
}
