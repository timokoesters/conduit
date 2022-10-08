use crate::{
    api::client_server::{self, claim_keys_helper, get_keys_helper},
    service::pdu::{gen_event_id_canonical_json, PduBuilder},
    services, utils, Error, PduEvent, Result, Ruma,
};
use axum::{response::IntoResponse, Json};
use futures_util::{StreamExt};
use get_profile_information::v1::ProfileField;
use http::header::{HeaderValue, AUTHORIZATION};

use ruma::{
    api::{
        client::error::{Error as RumaError, ErrorKind},
        federation::{
            authorization::get_event_authorization,
            device::get_devices::{self, v1::UserDevice},
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                get_server_keys,
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
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            member::{MembershipState, RoomMemberEventContent},
        },
        RoomEventType, StateEventType,
    },
    receipt::ReceiptType,
    serde::{Base64, JsonObject, Raw},
    signatures::{CanonicalJsonValue},
    to_device::DeviceIdOrAllDevices, EventId, MilliSecondsSinceUnixEpoch, RoomId, ServerName,
    ServerSigningKeyId,
};
use serde_json::value::{to_raw_value, RawValue as RawJsonValue};
use std::{
    collections::{BTreeMap},
    fmt::Debug,
    mem,
    net::{IpAddr, SocketAddr},
    sync::{Arc, RwLock},
    time::{Duration, Instant, SystemTime},
};

use tracing::{info, warn};

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

#[tracing::instrument(skip(request))]
pub(crate) async fn send_request<T: OutgoingRequest>(
    destination: &ServerName,
    request: T,
) -> Result<T::IncomingResponse>
where
    T: Debug,
{
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut write_destination_to_cache = false;

    let cached_result = services()
        .globals
        .actual_destination_cache
        .read()
        .unwrap()
        .get(destination)
        .cloned();

    let (actual_destination, host) = if let Some(result) = cached_result {
        result
    } else {
        write_destination_to_cache = true;

        let result = find_actual_destination(destination).await;

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
    request_map.insert(
        "origin".to_owned(),
        services().globals.server_name().as_str().into(),
    );
    request_map.insert("destination".to_owned(), destination.as_str().into());

    let mut request_json =
        serde_json::from_value(request_map.into()).expect("valid JSON is valid BTreeMap");

    ruma::signatures::sign_json(
        services().globals.server_name().as_str(),
        services().globals.keypair(),
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
                    services().globals.server_name(),
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

    let response = services()
        .globals
        .federation_client()
        .execute(reqwest_request)
        .await;

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
                    services()
                        .globals
                        .actual_destination_cache
                        .write()
                        .unwrap()
                        .insert(
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
async fn find_actual_destination(destination: &'_ ServerName) -> (FedDest, FedDest) {
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
                match request_well_known(destination.as_str()).await {
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
                                        query_srv_record(&delegated_hostname).await
                                    {
                                        // 3.3: SRV lookup successful
                                        let force_port = hostname_override.port();

                                        if let Ok(override_ip) = services()
                                            .globals
                                            .dns_resolver()
                                            .lookup_ip(hostname_override.hostname())
                                            .await
                                        {
                                            services()
                                                .globals
                                                .tls_name_override
                                                .write()
                                                .unwrap()
                                                .insert(
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
                        match query_srv_record(&destination_str).await {
                            // 4: SRV record found
                            Some(hostname_override) => {
                                let force_port = hostname_override.port();

                                if let Ok(override_ip) = services()
                                    .globals
                                    .dns_resolver()
                                    .lookup_ip(hostname_override.hostname())
                                    .await
                                {
                                    services()
                                        .globals
                                        .tls_name_override
                                        .write()
                                        .unwrap()
                                        .insert(
                                            hostname.clone(),
                                            (
                                                override_ip.iter().collect(),
                                                force_port.unwrap_or(8448),
                                            ),
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

async fn query_srv_record(hostname: &'_ str) -> Option<FedDest> {
    if let Ok(Some(host_port)) = services()
        .globals
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

async fn request_well_known(destination: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(
        &services()
            .globals
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
    _body: Ruma<get_server_version::v1::Request>,
) -> Result<get_server_version::v1::Response> {
    if !services().globals.allow_federation() {
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
pub async fn get_server_keys_route() -> Result<impl IntoResponse> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut verify_keys: BTreeMap<Box<ServerSigningKeyId>, VerifyKey> = BTreeMap::new();
    verify_keys.insert(
        format!("ed25519:{}", services().globals.keypair().version())
            .try_into()
            .expect("found invalid server signing keys in DB"),
        VerifyKey {
            key: Base64::new(services().globals.keypair().public_key().to_vec()),
        },
    );
    let mut response = serde_json::from_slice(
        get_server_keys::v2::Response {
            server_key: Raw::new(&ServerSigningKeys {
                server_name: services().globals.server_name().to_owned(),
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
        services().globals.server_name().as_str(),
        services().globals.keypair(),
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
pub async fn get_server_keys_deprecated_route() -> impl IntoResponse {
    get_server_keys_route().await
}

/// # `POST /_matrix/federation/v1/publicRooms`
///
/// Lists the public rooms on this server.
pub async fn get_public_rooms_filtered_route(
    body: Ruma<get_public_rooms_filtered::v1::IncomingRequest>,
) -> Result<get_public_rooms_filtered::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let response = client_server::get_public_rooms_filtered_helper(
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
    body: Ruma<get_public_rooms::v1::IncomingRequest>,
) -> Result<get_public_rooms::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let response = client_server::get_public_rooms_filtered_helper(
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
    body: Ruma<send_transaction_message::v1::IncomingRequest>,
) -> Result<send_transaction_message::v1::Response> {
    if !services().globals.allow_federation() {
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
        let (event_id, value) = match gen_event_id_canonical_json(pdu) {
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
                resolved_map.insert(
                    event_id,
                    Err(Error::bad_database("Event needs a valid RoomId.")),
                );
                continue;
            }
        };

        services()
            .rooms
            .event_handler
            .acl_check(&sender_servername, &room_id)?;

        let mutex = Arc::clone(
            services()
                .globals
                .roomid_mutex_federation
                .write()
                .unwrap()
                .entry(room_id.to_owned())
                .or_default(),
        );
        let mutex_lock = mutex.lock().await;
        let start_time = Instant::now();
        resolved_map.insert(
            event_id.clone(),
            services()
                .rooms
                .event_handler
                .handle_incoming_pdu(
                    &sender_servername,
                    &event_id,
                    &room_id,
                    value,
                    true,
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
            if matches!(e, Error::BadRequest(ErrorKind::NotFound, _)) {
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
                                services()
                                    .rooms
                                    .timeline
                                    .get_pdu_count(id)
                                    .ok()
                                    .flatten()
                                    .map(|r| (id, r))
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
                            services()
                                .rooms
                                .edus
                                .read_receipt
                                .readreceipt_update(&user_id, &room_id, event)?;
                        } else {
                            // TODO fetch missing events
                            info!("No known event ids in read receipt: {:?}", user_updates);
                        }
                    }
                }
            }
            Edu::Typing(typing) => {
                if services()
                    .rooms
                    .state_cache
                    .is_joined(&typing.user_id, &typing.room_id)?
                {
                    if typing.typing {
                        services().rooms.edus.typing.typing_add(
                            &typing.user_id,
                            &typing.room_id,
                            3000 + utils::millis_since_unix_epoch(),
                        )?;
                    } else {
                        services()
                            .rooms
                            .edus
                            .typing
                            .typing_remove(&typing.user_id, &typing.room_id)?;
                    }
                }
            }
            Edu::DeviceListUpdate(DeviceListUpdateContent { user_id, .. }) => {
                services().users.mark_device_key_update(&user_id)?;
            }
            Edu::DirectToDevice(DirectDeviceContent {
                sender,
                ev_type,
                message_id,
                messages,
            }) => {
                // Check if this is a new transaction id
                if services()
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
                                services().users.add_to_device_event(
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
                                )?
                            }

                            DeviceIdOrAllDevices::AllDevices => {
                                for target_device_id in
                                    services().users.all_device_ids(target_user_id)
                                {
                                    services().users.add_to_device_event(
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
                                    )?;
                                }
                            }
                        }
                    }
                }

                // Save transaction id with empty data
                services()
                    .transaction_ids
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
                    services().users.add_cross_signing_keys(
                        &user_id,
                        &master_key,
                        &self_signing_key,
                        &None,
                    )?;
                }
            }
            Edu::_Custom(_) => {}
        }
    }

    Ok(send_transaction_message::v1::Response {
        pdus: resolved_map
            .into_iter()
            .map(|(e, r)| (e, r.map_err(|e| e.to_string())))
            .collect(),
    })
}

/// # `GET /_matrix/federation/v1/event/{eventId}`
///
/// Retrieves a single event from the server.
///
/// - Only works if a user of this server is currently invited or joined the room
pub async fn get_event_route(
    body: Ruma<get_event::v1::IncomingRequest>,
) -> Result<get_event::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    let event = services()
        .rooms
        .timeline
        .get_pdu_json(&body.event_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Event not found."))?;

    let room_id_str = event
        .get("room_id")
        .and_then(|val| val.as_str())
        .ok_or_else(|| Error::bad_database("Invalid event in database"))?;

    let room_id = <&RoomId>::try_from(room_id_str)
        .map_err(|_| Error::bad_database("Invalid room id field in event in database"))?;

    if !services()
        .rooms
        .state_cache
        .server_in_room(sender_servername, room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room",
        ));
    }

    Ok(get_event::v1::Response {
        origin: services().globals.server_name().to_owned(),
        origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
        pdu: PduEvent::convert_to_outgoing_federation_event(event),
    })
}

/// # `POST /_matrix/federation/v1/get_missing_events/{roomId}`
///
/// Retrieves events that the sender is missing.
pub async fn get_missing_events_route(
    body: Ruma<get_missing_events::v1::IncomingRequest>,
) -> Result<get_missing_events::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    if !services()
        .rooms
        .state_cache
        .server_in_room(sender_servername, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(&sender_servername, &body.room_id)?;

    let mut queued_events = body.latest_events.clone();
    let mut events = Vec::new();

    let mut i = 0;
    while i < queued_events.len() && events.len() < u64::from(body.limit) as usize {
        if let Some(pdu) = services().rooms.timeline.get_pdu_json(&queued_events[i])? {
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
    body: Ruma<get_event_authorization::v1::IncomingRequest>,
) -> Result<get_event_authorization::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    if !services()
        .rooms
        .state_cache
        .server_in_room(sender_servername, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room.",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(&sender_servername, &body.room_id)?;

    let event = services()
        .rooms
        .timeline
        .get_pdu_json(&body.event_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Event not found."))?;

    let room_id_str = event
        .get("room_id")
        .and_then(|val| val.as_str())
        .ok_or_else(|| Error::bad_database("Invalid event in database"))?;

    let room_id = <&RoomId>::try_from(room_id_str)
        .map_err(|_| Error::bad_database("Invalid room id field in event in database"))?;

    let auth_chain_ids = services()
        .rooms
        .auth_chain
        .get_auth_chain(room_id, vec![Arc::from(&*body.event_id)])
        .await?;

    Ok(get_event_authorization::v1::Response {
        auth_chain: auth_chain_ids
            .filter_map(|id| services().rooms.timeline.get_pdu_json(&id).ok()?)
            .map(PduEvent::convert_to_outgoing_federation_event)
            .collect(),
    })
}

/// # `GET /_matrix/federation/v1/state/{roomId}`
///
/// Retrieves the current state of the room.
pub async fn get_room_state_route(
    body: Ruma<get_room_state::v1::IncomingRequest>,
) -> Result<get_room_state::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    if !services()
        .rooms
        .state_cache
        .server_in_room(sender_servername, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room.",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(&sender_servername, &body.room_id)?;

    let shortstatehash = services()
        .rooms
        .state_accessor
        .pdu_shortstatehash(&body.event_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Pdu state not found.",
        ))?;

    let pdus = services()
        .rooms
        .state_accessor
        .state_full_ids(shortstatehash)
        .await?
        .into_iter()
        .map(|(_, id)| {
            PduEvent::convert_to_outgoing_federation_event(
                services()
                    .rooms
                    .timeline
                    .get_pdu_json(&id)
                    .unwrap()
                    .unwrap(),
            )
        })
        .collect();

    let auth_chain_ids = services()
        .rooms
        .auth_chain
        .get_auth_chain(&body.room_id, vec![Arc::from(&*body.event_id)])
        .await?;

    Ok(get_room_state::v1::Response {
        auth_chain: auth_chain_ids
            .map(|id| {
                services()
                    .rooms
                    .timeline
                    .get_pdu_json(&id)
                    .map(|maybe_json| {
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
    body: Ruma<get_room_state_ids::v1::IncomingRequest>,
) -> Result<get_room_state_ids::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    if !services()
        .rooms
        .state_cache
        .server_in_room(sender_servername, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Server is not in room.",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(&sender_servername, &body.room_id)?;

    let shortstatehash = services()
        .rooms
        .state_accessor
        .pdu_shortstatehash(&body.event_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Pdu state not found.",
        ))?;

    let pdu_ids = services()
        .rooms
        .state_accessor
        .state_full_ids(shortstatehash)
        .await?
        .into_iter()
        .map(|(_, id)| (*id).to_owned())
        .collect();

    let auth_chain_ids = services()
        .rooms
        .auth_chain
        .get_auth_chain(&body.room_id, vec![Arc::from(&*body.event_id)])
        .await?;

    Ok(get_room_state_ids::v1::Response {
        auth_chain_ids: auth_chain_ids.map(|id| (*id).to_owned()).collect(),
        pdu_ids,
    })
}

/// # `GET /_matrix/federation/v1/make_join/{roomId}/{userId}`
///
/// Creates a join template.
pub async fn create_join_event_template_route(
    body: Ruma<prepare_join_event::v1::IncomingRequest>,
) -> Result<prepare_join_event::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    if !services().rooms.metadata.exists(&body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Room is unknown to this server.",
        ));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    services()
        .rooms
        .event_handler
        .acl_check(&sender_servername, &body.room_id)?;

    let mutex_state = Arc::clone(
        services()
            .globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(body.room_id.to_owned())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    // TODO: Conduit does not implement restricted join rules yet, we always reject
    let join_rules_event = services().rooms.state_accessor.room_state_get(
        &body.room_id,
        &StateEventType::RoomJoinRules,
        "",
    )?;

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

    let room_version_id = services().rooms.state.get_room_version(&body.room_id)?;
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

    let (_pdu, pdu_json) = services().rooms.timeline.create_hash_and_sign_event(
        PduBuilder {
            event_type: RoomEventType::RoomMember,
            content,
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &body.user_id,
        &body.room_id,
        &state_lock,
    )?;

    drop(state_lock);

    Ok(prepare_join_event::v1::Response {
        room_version: Some(room_version_id),
        event: to_raw_value(&pdu_json).expect("CanonicalJson can be serialized to JSON"),
    })
}

async fn create_join_event(
    sender_servername: &ServerName,
    room_id: &RoomId,
    pdu: &RawJsonValue,
) -> Result<RoomState> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    if !services().rooms.metadata.exists(room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Room is unknown to this server.",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(&sender_servername, room_id)?;

    // TODO: Conduit does not implement restricted join rules yet, we always reject
    let join_rules_event = services().rooms.state_accessor.room_state_get(
        room_id,
        &StateEventType::RoomJoinRules,
        "",
    )?;

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
    let shortstatehash = services()
        .rooms
        .state
        .get_room_shortstatehash(room_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Pdu state not found.",
        ))?;

    let pub_key_map = RwLock::new(BTreeMap::new());
    // let mut auth_cache = EventMap::new();

    // We do not add the event_id field to the pdu here because of signature and hashes checks
    let (event_id, value) = match gen_event_id_canonical_json(pdu) {
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
        services()
            .globals
            .roomid_mutex_federation
            .write()
            .unwrap()
            .entry(room_id.to_owned())
            .or_default(),
    );
    let mutex_lock = mutex.lock().await;
    let pdu_id: Vec<u8> = services()
        .rooms
        .event_handler
        .handle_incoming_pdu(&origin, &event_id, room_id, value, true, &pub_key_map)
        .await?
        .ok_or(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Could not accept incoming PDU as timeline event.",
        ))?;
    drop(mutex_lock);

    let state_ids = services()
        .rooms
        .state_accessor
        .state_full_ids(shortstatehash)
        .await?;
    let auth_chain_ids = services()
        .rooms
        .auth_chain
        .get_auth_chain(
            room_id,
            state_ids.iter().map(|(_, id)| id.clone()).collect(),
        )
        .await?;

    let servers = services()
        .rooms
        .state_cache
        .room_servers(room_id)
        .filter_map(|r| r.ok())
        .filter(|server| &**server != services().globals.server_name());

    services().sending.send_pdu(servers, &pdu_id)?;

    Ok(RoomState {
        auth_chain: auth_chain_ids
            .filter_map(|id| services().rooms.timeline.get_pdu_json(&id).ok().flatten())
            .map(PduEvent::convert_to_outgoing_federation_event)
            .collect(),
        state: state_ids
            .iter()
            .filter_map(|(_, id)| services().rooms.timeline.get_pdu_json(id).ok().flatten())
            .map(PduEvent::convert_to_outgoing_federation_event)
            .collect(),
    })
}

/// # `PUT /_matrix/federation/v1/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub async fn create_join_event_v1_route(
    body: Ruma<create_join_event::v1::IncomingRequest>,
) -> Result<create_join_event::v1::Response> {
    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    let room_state = create_join_event(sender_servername, &body.room_id, &body.pdu).await?;

    Ok(create_join_event::v1::Response { room_state })
}

/// # `PUT /_matrix/federation/v2/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub async fn create_join_event_v2_route(
    body: Ruma<create_join_event::v2::IncomingRequest>,
) -> Result<create_join_event::v2::Response> {
    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    let room_state = create_join_event(sender_servername, &body.room_id, &body.pdu).await?;

    Ok(create_join_event::v2::Response { room_state })
}

/// # `PUT /_matrix/federation/v2/invite/{roomId}/{eventId}`
///
/// Invites a remote user to a room.
pub async fn create_invite_route(
    body: Ruma<create_invite::v2::IncomingRequest>,
) -> Result<create_invite::v2::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    services()
        .rooms
        .event_handler
        .acl_check(&sender_servername, &body.room_id)?;

    if !services()
        .globals
        .supported_room_versions()
        .contains(&body.room_version)
    {
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
        services().globals.server_name().as_str(),
        services().globals.keypair(),
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
    if !services().rooms.metadata.exists(&pdu.room_id)? {
        services().rooms.state_cache.update_membership(
            &body.room_id,
            &invited_user,
            MembershipState::Invite,
            &sender,
            Some(invite_state),
            true,
        )?;
    }

    Ok(create_invite::v2::Response {
        event: PduEvent::convert_to_outgoing_federation_event(signed_event),
    })
}

/// # `GET /_matrix/federation/v1/user/devices/{userId}`
///
/// Gets information on all devices of the user.
pub async fn get_devices_route(
    body: Ruma<get_devices::v1::IncomingRequest>,
) -> Result<get_devices::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    Ok(get_devices::v1::Response {
        user_id: body.user_id.clone(),
        stream_id: services()
            .users
            .get_devicelist_version(&body.user_id)?
            .unwrap_or(0)
            .try_into()
            .expect("version will not grow that large"),
        devices: services()
            .users
            .all_devices_metadata(&body.user_id)
            .filter_map(|r| r.ok())
            .filter_map(|metadata| {
                Some(UserDevice {
                    keys: services()
                        .users
                        .get_device_keys(&body.user_id, &metadata.device_id)
                        .ok()??,
                    device_id: metadata.device_id,
                    device_display_name: metadata.display_name,
                })
            })
            .collect(),
        master_key: services()
            .users
            .get_master_key(&body.user_id, &|u| u.server_name() == sender_servername)?,
        self_signing_key: services()
            .users
            .get_self_signing_key(&body.user_id, &|u| u.server_name() == sender_servername)?,
    })
}

/// # `GET /_matrix/federation/v1/query/directory`
///
/// Resolve a room alias to a room id.
pub async fn get_room_information_route(
    body: Ruma<get_room_information::v1::IncomingRequest>,
) -> Result<get_room_information::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let room_id = services()
        .rooms
        .alias
        .resolve_local_alias(&body.room_alias)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Room alias not found.",
        ))?;

    Ok(get_room_information::v1::Response {
        room_id,
        servers: vec![services().globals.server_name().to_owned()],
    })
}

/// # `GET /_matrix/federation/v1/query/profile`
///
/// Gets information on a profile.
pub async fn get_profile_information_route(
    body: Ruma<get_profile_information::v1::IncomingRequest>,
) -> Result<get_profile_information::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut displayname = None;
    let mut avatar_url = None;
    let mut blurhash = None;

    match &body.field {
        Some(ProfileField::DisplayName) => {
            displayname = services().users.displayname(&body.user_id)?
        }
        Some(ProfileField::AvatarUrl) => {
            avatar_url = services().users.avatar_url(&body.user_id)?;
            blurhash = services().users.blurhash(&body.user_id)?
        }
        // TODO: what to do with custom
        Some(_) => {}
        None => {
            displayname = services().users.displayname(&body.user_id)?;
            avatar_url = services().users.avatar_url(&body.user_id)?;
            blurhash = services().users.blurhash(&body.user_id)?;
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
pub async fn get_keys_route(body: Ruma<get_keys::v1::Request>) -> Result<get_keys::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let result = get_keys_helper(None, &body.device_keys, |u| {
        Some(u.server_name()) == body.sender_servername.as_deref()
    })
    .await?;

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
    body: Ruma<claim_keys::v1::Request>,
) -> Result<claim_keys::v1::Response> {
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let result = claim_keys_helper(&body.one_time_keys).await?;

    Ok(claim_keys::v1::Response {
        one_time_keys: result.one_time_keys,
    })
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
