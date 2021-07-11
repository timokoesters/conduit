use crate::{
    client_server::{self, claim_keys_helper, get_keys_helper},
    database::DatabaseGuard,
    utils, ConduitResult, Database, Error, PduEvent, Result, Ruma,
};
use get_profile_information::v1::ProfileField;
use http::header::{HeaderValue, AUTHORIZATION, HOST};
use log::{debug, error, info, trace, warn};
use regex::Regex;
use rocket::response::content::Json;
use ruma::{
    api::{
        client::error::{Error as RumaError, ErrorKind},
        federation::{
            authorization::get_event_authorization,
            device::get_devices::{self, v1::UserDevice},
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                get_remote_server_keys, get_server_keys, get_server_version, ServerSigningKeys,
                VerifyKey,
            },
            event::{get_event, get_missing_events, get_room_state, get_room_state_ids},
            keys::{claim_keys, get_keys},
            membership::{
                create_invite,
                create_join_event::{self, RoomState},
                create_join_event_template,
            },
            query::{get_profile_information, get_room_information},
            transactions::{
                edu::{DirectDeviceContent, Edu},
                send_transaction_message,
            },
        },
        EndpointError, IncomingResponse, OutgoingRequest, OutgoingResponse, SendAccessToken,
    },
    directory::{IncomingFilter, IncomingRoomNetwork},
    events::{
        receipt::{ReceiptEvent, ReceiptEventContent},
        room::{
            create::CreateEventContent,
            member::{MemberEventContent, MembershipState},
        },
        AnyEphemeralRoomEvent, EventType,
    },
    receipt::ReceiptType,
    serde::Raw,
    signatures::{CanonicalJsonObject, CanonicalJsonValue},
    state_res::{self, Event, RoomVersion, StateMap},
    to_device::DeviceIdOrAllDevices,
    uint, EventId, MilliSecondsSinceUnixEpoch, RoomId, RoomVersionId, ServerName,
    ServerSigningKeyId, UserId,
};
use std::{
    collections::{btree_map::Entry, BTreeMap, BTreeSet, HashSet},
    convert::{TryFrom, TryInto},
    fmt::Debug,
    future::Future,
    mem,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    result::Result as StdResult,
    sync::{Arc, RwLock},
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::Semaphore;

#[cfg(feature = "conduit_bin")]
use rocket::{get, post, put};

/// Wraps either an literal IP address plus port, or a hostname plus complement
/// (colon-plus-port if it was specified).
///
/// Note: A `FedDest::Named` might contain an IP address in string form if there
/// was no port specified to construct a SocketAddr with.
///
/// # Examples:
/// ```rust,ignore
/// FedDest::Literal("198.51.100.3:8448".parse()?);
/// FedDest::Literal("[2001:db8::4:5]:443".parse()?);
/// FedDest::Named("matrix.example.org".to_owned(), "".to_owned());
/// FedDest::Named("matrix.example.org".to_owned(), ":8448".to_owned());
/// FedDest::Named("198.51.100.5".to_owned(), "".to_owned());
/// ```
#[derive(Clone, Debug, PartialEq)]
enum FedDest {
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
}

#[tracing::instrument(skip(globals))]
pub async fn send_request<T: OutgoingRequest>(
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

    let maybe_result = globals
        .actual_destination_cache
        .read()
        .unwrap()
        .get(destination)
        .cloned();

    let (actual_destination, host) = if let Some(result) = maybe_result {
        result
    } else {
        let result = find_actual_destination(globals, &destination).await;
        let (actual_destination, host) = result.clone();
        let result_string = (result.0.into_https_string(), result.1.into_uri_string());
        globals
            .actual_destination_cache
            .write()
            .unwrap()
            .insert(Box::<ServerName>::from(destination), result_string.clone());
        let dest_hostname = actual_destination.hostname();
        let host_hostname = host.hostname();
        if dest_hostname != host_hostname {
            globals.tls_name_override.write().unwrap().insert(
                dest_hostname,
                webpki::DNSNameRef::try_from_ascii_str(&host_hostname)
                    .unwrap()
                    .to_owned(),
            );
        }
        result_string
    };

    let mut http_request = request
        .try_into_http_request::<Vec<u8>>(&actual_destination, SendAccessToken::IfRequired(""))
        .map_err(|e| {
            warn!("Failed to find destination {}: {}", actual_destination, e);
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

    http_request
        .headers_mut()
        .insert(HOST, HeaderValue::from_str(&host).unwrap());

    let mut reqwest_request = reqwest::Request::try_from(http_request)
        .expect("all http requests are valid reqwest requests");

    *reqwest_request.timeout_mut() = Some(Duration::from_secs(30));

    let url = reqwest_request.url().clone();
    let response = globals.reqwest_client().execute(reqwest_request).await;

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
                info!(
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
                response.map_err(|_| Error::BadServerResponse("Server returned bad 200 response."))
            } else {
                Err(Error::FederationError(
                    destination.to_owned(),
                    RumaError::try_from_http_response(http_response).map_err(|_| {
                        Error::BadServerResponse("Server returned bad error response.")
                    })?,
                ))
            }
        }
        Err(e) => Err(e.into()),
    }
}

#[tracing::instrument]
fn get_ip_with_port(destination_str: &str) -> Option<FedDest> {
    if let Ok(destination) = destination_str.parse::<SocketAddr>() {
        Some(FedDest::Literal(destination))
    } else if let Ok(ip_addr) = destination_str.parse::<IpAddr>() {
        Some(FedDest::Literal(SocketAddr::new(ip_addr, 8448)))
    } else {
        None
    }
}

#[tracing::instrument]
fn add_port_to_hostname(destination_str: &str) -> FedDest {
    let (host, port) = match destination_str.find(':') {
        None => (destination_str, ":8448"),
        Some(pos) => destination_str.split_at(pos),
    };
    FedDest::Named(host.to_string(), port.to_string())
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
                FedDest::Named(host.to_string(), port.to_string())
            } else {
                match request_well_known(globals, &destination.as_str()).await {
                    // 3: A .well-known file is available
                    Some(delegated_hostname) => {
                        hostname = delegated_hostname.clone();
                        match get_ip_with_port(&delegated_hostname) {
                            Some(host_and_port) => host_and_port, // 3.1: IP literal in .well-known file
                            None => {
                                if let Some(pos) = destination_str.find(':') {
                                    // 3.2: Hostname with port in .well-known file
                                    let (host, port) = destination_str.split_at(pos);
                                    FedDest::Named(host.to_string(), port.to_string())
                                } else {
                                    match query_srv_record(globals, &delegated_hostname).await {
                                        // 3.3: SRV lookup successful
                                        Some(hostname) => hostname,
                                        // 3.4: No SRV records, just use the hostname from .well-known
                                        None => add_port_to_hostname(&delegated_hostname),
                                    }
                                }
                            }
                        }
                    }
                    // 4: No .well-known or an error occured
                    None => {
                        match query_srv_record(globals, &destination_str).await {
                            // 4: SRV record found
                            Some(hostname) => hostname,
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
        FedDest::Named(addr.to_string(), "".to_string())
    } else if let Some(pos) = hostname.find(':') {
        let (host, port) = hostname.split_at(pos);
        FedDest::Named(host.to_string(), port.to_string())
    } else {
        FedDest::Named(hostname, "".to_string())
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
                    result
                        .target()
                        .to_string()
                        .trim_end_matches('.')
                        .to_string(),
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
pub async fn request_well_known(
    globals: &crate::database::globals::Globals,
    destination: &str,
) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(
        &globals
            .reqwest_client()
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

#[cfg_attr(feature = "conduit_bin", get("/_matrix/federation/v1/version"))]
#[tracing::instrument(skip(db))]
pub fn get_server_version_route(
    db: DatabaseGuard,
) -> ConduitResult<get_server_version::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    Ok(get_server_version::v1::Response {
        server: Some(get_server_version::v1::Server {
            name: Some("Conduit".to_owned()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
    }
    .into())
}

// Response type for this endpoint is Json because we need to calculate a signature for the response
#[cfg_attr(feature = "conduit_bin", get("/_matrix/key/v2/server"))]
#[tracing::instrument(skip(db))]
pub fn get_server_keys_route(db: DatabaseGuard) -> Json<String> {
    if !db.globals.allow_federation() {
        // TODO: Use proper types
        return Json("Federation is disabled.".to_owned());
    }

    let mut verify_keys = BTreeMap::new();
    verify_keys.insert(
        ServerSigningKeyId::try_from(
            format!("ed25519:{}", db.globals.keypair().version()).as_str(),
        )
        .expect("found invalid server signing keys in DB"),
        VerifyKey {
            key: base64::encode_config(db.globals.keypair().public_key(), base64::STANDARD_NO_PAD),
        },
    );
    let mut response = serde_json::from_slice(
        get_server_keys::v2::Response {
            server_key: ServerSigningKeys {
                server_name: db.globals.server_name().to_owned(),
                verify_keys,
                old_verify_keys: BTreeMap::new(),
                signatures: BTreeMap::new(),
                valid_until_ts: MilliSecondsSinceUnixEpoch::from_system_time(
                    SystemTime::now() + Duration::from_secs(60 * 2),
                )
                .expect("time is valid"),
            },
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

    Json(ruma::serde::to_canonical_json_string(&response).expect("JSON is canonical"))
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/key/v2/server/<_>"))]
#[tracing::instrument(skip(db))]
pub fn get_server_keys_deprecated_route(db: DatabaseGuard) -> Json<String> {
    get_server_keys_route(db)
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/federation/v1/publicRooms", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_public_rooms_filtered_route(
    db: DatabaseGuard,
    body: Ruma<get_public_rooms_filtered::v1::Request<'_>>,
) -> ConduitResult<get_public_rooms_filtered::v1::Response> {
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
    .await?
    .0;

    Ok(get_public_rooms_filtered::v1::Response {
        chunk: response
            .chunk
            .into_iter()
            .map(|c| {
                // Convert ruma::api::federation::directory::get_public_rooms::v1::PublicRoomsChunk
                // to ruma::api::client::r0::directory::PublicRoomsChunk
                serde_json::from_str(
                    &serde_json::to_string(&c).expect("PublicRoomsChunk::to_string always works"),
                )
                .expect("federation and client-server PublicRoomsChunk are the same type")
            })
            .collect(),
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/publicRooms", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_public_rooms_route(
    db: DatabaseGuard,
    body: Ruma<get_public_rooms::v1::Request<'_>>,
) -> ConduitResult<get_public_rooms::v1::Response> {
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
    .await?
    .0;

    Ok(get_public_rooms::v1::Response {
        chunk: response
            .chunk
            .into_iter()
            .map(|c| {
                // Convert ruma::api::federation::directory::get_public_rooms::v1::PublicRoomsChunk
                // to ruma::api::client::r0::directory::PublicRoomsChunk
                serde_json::from_str(
                    &serde_json::to_string(&c).expect("PublicRoomsChunk::to_string always works"),
                )
                .expect("federation and client-server PublicRoomsChunk are the same type")
            })
            .collect(),
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/federation/v1/send/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn send_transaction_message_route(
    db: DatabaseGuard,
    body: Ruma<send_transaction_message::v1::Request<'_>>,
) -> ConduitResult<send_transaction_message::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

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
        let (event_id, value) = match crate::pdu::gen_event_id_canonical_json(pdu) {
            Ok(t) => t,
            Err(_) => {
                // Event could not be converted to canonical json
                continue;
            }
        };

        let start_time = Instant::now();
        resolved_map.insert(
            event_id.clone(),
            handle_incoming_pdu(&body.origin, &event_id, value, true, &db, &pub_key_map)
                .await
                .map(|_| ()),
        );

        let elapsed = start_time.elapsed();
        if elapsed > Duration::from_secs(1) {
            warn!(
                "Handling event {} took {}m{}s",
                event_id,
                elapsed.as_secs() / 60,
                elapsed.as_secs() % 60
            );
        }
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
        .map(|edu| serde_json::from_str::<Edu>(edu.json().get()))
        .filter_map(|r| r.ok())
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
                                db.rooms.get_pdu_count(&id).ok().flatten().map(|r| (id, r))
                            })
                            .max_by_key(|(_, count)| *count)
                        {
                            let mut user_receipts = BTreeMap::new();
                            user_receipts.insert(user_id.clone(), user_updates.data);

                            let mut receipts = BTreeMap::new();
                            receipts.insert(ReceiptType::Read, user_receipts);

                            let mut receipt_content = BTreeMap::new();
                            receipt_content.insert(event_id.to_owned(), receipts);

                            let event = AnyEphemeralRoomEvent::Receipt(ReceiptEvent {
                                content: ReceiptEventContent(receipt_content),
                                room_id: room_id.clone(),
                            });
                            db.rooms.edus.readreceipt_update(
                                &user_id,
                                &room_id,
                                event,
                                &db.globals,
                            )?;
                        } else {
                            warn!("No known event ids in read receipt: {:?}", user_updates);
                        }
                    }
                }
            }
            Edu::Typing(typing) => {
                if typing.typing {
                    db.rooms.edus.typing_add(
                        &typing.user_id,
                        &typing.room_id,
                        3000 + utils::millis_since_unix_epoch(),
                        &db.globals,
                    )?;
                } else {
                    db.rooms
                        .edus
                        .typing_remove(&typing.user_id, &typing.room_id, &db.globals)?;
                }
            }
            Edu::DeviceListUpdate(_) => {
                // TODO: Instead of worrying about stream ids we can just fetch all devices again
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
                                    &target_user_id,
                                    &target_device_id,
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
                                for target_device_id in db.users.all_device_ids(&target_user_id) {
                                    db.users.add_to_device_event(
                                        &sender,
                                        &target_user_id,
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
            Edu::_Custom(_) => {}
        }
    }

    db.flush().await?;

    Ok(send_transaction_message::v1::Response { pdus: resolved_map }.into())
}

/// An async function that can recursively call itself.
type AsyncRecursiveResult<'a, T, E> = Pin<Box<dyn Future<Output = StdResult<T, E>> + 'a + Send>>;

/// When receiving an event one needs to:
/// 0. Skip the PDU if we already know about it
/// 1. Check the server is in the room
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
// We use some AsyncRecursiveResult hacks here so we can call this async funtion recursively
pub fn handle_incoming_pdu<'a>(
    origin: &'a ServerName,
    event_id: &'a EventId,
    value: BTreeMap<String, CanonicalJsonValue>,
    is_timeline_event: bool,
    db: &'a Database,
    pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, String>>>,
) -> AsyncRecursiveResult<'a, Option<Vec<u8>>, String> {
    Box::pin(async move {
        // TODO: For RoomVersion6 we must check that Raw<..> is canonical do we anywhere?: https://matrix.org/docs/spec/rooms/v6#canonical-json

        // 0. Skip the PDU if we already have it as a timeline event
        if let Ok(Some(pdu_id)) = db.rooms.get_pdu_id(&event_id) {
            return Ok(Some(pdu_id.to_vec()));
        }

        // 1. Check the server is in the room
        let room_id = match value
            .get("room_id")
            .and_then(|id| RoomId::try_from(id.as_str()?).ok())
        {
            Some(id) => id,
            None => {
                // Event is invalid
                return Err("Event needs a valid RoomId.".to_string());
            }
        };

        match db.rooms.exists(&room_id) {
            Ok(true) => {}
            _ => {
                return Err("Room is unknown to this server.".to_string());
            }
        }

        // We go through all the signatures we see on the value and fetch the corresponding signing
        // keys
        fetch_required_signing_keys(&value, &pub_key_map, db)
            .await
            .map_err(|e| e.to_string())?;

        // 2. Check signatures, otherwise drop
        // 3. check content hash, redact if doesn't match
        let create_event = db
            .rooms
            .room_state_get(&room_id, &EventType::RoomCreate, "")
            .map_err(|_| "Failed to ask database for event.".to_owned())?
            .ok_or_else(|| "Failed to find create event in db.".to_owned())?;

        let create_event_content =
            serde_json::from_value::<Raw<CreateEventContent>>(create_event.content.clone())
                .expect("Raw::from_value always works.")
                .deserialize()
                .map_err(|_| "Invalid PowerLevels event in db.".to_owned())?;

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
                return Err("Signature verification failed".to_string());
            }
            Ok(ruma::signatures::Verified::Signatures) => {
                // Redact
                warn!("Calculated hash does not match: {}", event_id);
                match ruma::signatures::redact(&value, room_version_id) {
                    Ok(obj) => obj,
                    Err(_) => return Err("Redaction failed".to_string()),
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
        .map_err(|_| "Event is not a valid PDU.".to_string())?;

        // 4. fetch any missing auth events doing all checks listed here starting at 1. These are not timeline events
        // 5. Reject "due to auth events" if can't get all the auth events or some of the auth events are also rejected "due to auth events"
        // EDIT: Step 5 is not applied anymore because it failed too often
        debug!("Fetching auth events for {}", incoming_pdu.event_id);
        fetch_and_handle_events(db, origin, &incoming_pdu.auth_events, pub_key_map)
            .await
            .map_err(|e| e.to_string())?;

        // 6. Reject "due to auth events" if the event doesn't pass auth based on the auth events
        debug!(
            "Auth check for {} based on auth events",
            incoming_pdu.event_id
        );

        // Build map of auth events
        let mut auth_events = BTreeMap::new();
        for id in &incoming_pdu.auth_events {
            let auth_event = db
                .rooms
                .get_pdu(id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| {
                    "Auth event not found, event failed recursive auth checks.".to_string()
                })?;

            match auth_events.entry((
                auth_event.kind.clone(),
                auth_event
                    .state_key
                    .clone()
                    .expect("all auth events have state keys"),
            )) {
                Entry::Vacant(v) => {
                    v.insert(auth_event.clone());
                }
                Entry::Occupied(_) => {
                    return Err(
                        "Auth event's type and state_key combination exists multiple times."
                            .to_owned(),
                    )
                }
            }
        }

        // The original create event must be in the auth events
        if auth_events
            .get(&(EventType::RoomCreate, "".to_owned()))
            .map(|a| a.as_ref())
            != Some(&create_event)
        {
            return Err("Incoming event refers to wrong create event.".to_owned());
        }

        // If the previous event was the create event special rules apply
        let previous_create = if incoming_pdu.auth_events.len() == 1
            && incoming_pdu.prev_events == incoming_pdu.auth_events
        {
            db.rooms
                .get_pdu(&incoming_pdu.auth_events[0])
                .map_err(|e| e.to_string())?
                .filter(|maybe_create| **maybe_create == *create_event)
        } else {
            None
        };

        let incoming_pdu = Arc::new(incoming_pdu.clone());

        if !state_res::event_auth::auth_check(
            &room_version,
            &incoming_pdu,
            previous_create.clone(),
            &auth_events,
            None, // TODO: third party invite
        )
        .map_err(|_e| "Auth check failed".to_string())?
        {
            return Err("Event has failed auth check with auth events.".to_string());
        }

        debug!("Validation successful.");

        // 7. Persist the event as an outlier.
        db.rooms
            .add_pdu_outlier(&incoming_pdu.event_id, &val)
            .map_err(|_| "Failed to add pdu as outlier.".to_owned())?;
        debug!("Added pdu as outlier.");

        // 8. if not timeline event: stop
        if !is_timeline_event {
            return Ok(None);
        }

        // TODO: 9. fetch any missing prev events doing all checks listed here starting at 1. These are timeline events

        // 10. Fetch missing state and auth chain events by calling /state_ids at backwards extremities
        //     doing all the checks in this list starting at 1. These are not timeline events.

        // TODO: if we know the prev_events of the incoming event we can avoid the request and build
        // the state from a known point and resolve if > 1 prev_event

        debug!("Requesting state at event.");
        let mut state_at_incoming_event = None;

        if incoming_pdu.prev_events.len() == 1 {
            let prev_event = &incoming_pdu.prev_events[0];
            let state_vec = db
                .rooms
                .pdu_shortstatehash(prev_event)
                .map_err(|_| "Failed talking to db".to_owned())?
                .map(|shortstatehash| db.rooms.state_full_ids(shortstatehash).ok())
                .flatten();
            if let Some(mut state_vec) = state_vec {
                if db
                    .rooms
                    .get_pdu(prev_event)
                    .ok()
                    .flatten()
                    .ok_or_else(|| "Could not find prev event, but we know the state.".to_owned())?
                    .state_key
                    .is_some()
                {
                    state_vec.push(prev_event.clone());
                }
                state_at_incoming_event = Some(
                    fetch_and_handle_events(db, origin, &state_vec, pub_key_map)
                        .await
                        .map_err(|_| "Failed to fetch state events locally".to_owned())?
                        .into_iter()
                        .map(|pdu| {
                            (
                                (
                                    pdu.kind.clone(),
                                    pdu.state_key
                                        .clone()
                                        .expect("events from state_full_ids are state events"),
                                ),
                                pdu,
                            )
                        })
                        .collect(),
                );
            }
            // TODO: set incoming_auth_events?
        }

        if state_at_incoming_event.is_none() {
            // Call /state_ids to find out what the state at this pdu is. We trust the server's
            // response to some extend, but we still do a lot of checks on the events
            match db
                .sending
                .send_federation_request(
                    &db.globals,
                    origin,
                    get_room_state_ids::v1::Request {
                        room_id: &room_id,
                        event_id: &incoming_pdu.event_id,
                    },
                )
                .await
            {
                Ok(res) => {
                    debug!("Fetching state events at event.");
                    let state_vec =
                        match fetch_and_handle_events(&db, origin, &res.pdu_ids, pub_key_map).await
                        {
                            Ok(state) => state,
                            Err(_) => return Err("Failed to fetch state events.".to_owned()),
                        };

                    let mut state = BTreeMap::new();
                    for pdu in state_vec {
                        match state.entry((pdu.kind.clone(), pdu.state_key.clone().ok_or_else(|| "Found non-state pdu in state events.".to_owned())?)) {
                            Entry::Vacant(v) => {
                                v.insert(pdu);
                            }
                            Entry::Occupied(_) => {
                                return Err(
                                    "State event's type and state_key combination exists multiple times.".to_owned(),
                                )
                            }
                        }
                    }

                    // The original create event must still be in the state
                    if state
                        .get(&(EventType::RoomCreate, "".to_owned()))
                        .map(|a| a.as_ref())
                        != Some(&create_event)
                    {
                        return Err("Incoming event refers to wrong create event.".to_owned());
                    }

                    debug!("Fetching auth chain events at event.");
                    match fetch_and_handle_events(&db, origin, &res.auth_chain_ids, pub_key_map)
                        .await
                    {
                        Ok(state) => state,
                        Err(_) => return Err("Failed to fetch auth chain.".to_owned()),
                    };

                    state_at_incoming_event = Some(state);
                }
                Err(_) => {
                    return Err("Fetching state for event failed".into());
                }
            };
        }

        let state_at_incoming_event =
            state_at_incoming_event.expect("we always set this to some above");

        // 11. Check the auth of the event passes based on the state of the event
        if !state_res::event_auth::auth_check(
            &room_version,
            &incoming_pdu,
            previous_create.clone(),
            &state_at_incoming_event,
            None, // TODO: third party invite
        )
        .map_err(|_e| "Auth check failed.".to_owned())?
        {
            return Err("Event has failed auth check with state at the event.".into());
        }
        debug!("Auth check succeeded.");

        // Now we calculate the set of extremities this room has after the incoming event has been
        // applied. We start with the previous extremities (aka leaves)
        let mut extremities = db
            .rooms
            .get_pdu_leaves(&room_id)
            .map_err(|_| "Failed to load room leaves".to_owned())?;

        // Remove any forward extremities that are referenced by this incoming event's prev_events
        for prev_event in &incoming_pdu.prev_events {
            if extremities.contains(prev_event) {
                extremities.remove(prev_event);
            }
        }

        let mut fork_states = BTreeSet::new();
        for id in &extremities {
            match db
                .rooms
                .get_pdu(&id)
                .map_err(|_| "Failed to ask db for pdu.".to_owned())?
            {
                Some(leaf_pdu) => {
                    let pdu_shortstatehash = db
                        .rooms
                        .pdu_shortstatehash(&leaf_pdu.event_id)
                        .map_err(|_| "Failed to ask db for pdu state hash.".to_owned())?
                        .ok_or_else(|| {
                            error!(
                                "Found extremity pdu with no statehash in db: {:?}",
                                leaf_pdu
                            );
                            "Found pdu with no statehash in db.".to_owned()
                        })?;

                    let mut leaf_state = db
                        .rooms
                        .state_full(pdu_shortstatehash)
                        .map_err(|_| "Failed to ask db for room state.".to_owned())?;

                    if let Some(state_key) = &leaf_pdu.state_key {
                        // Now it's the state after
                        let key = (leaf_pdu.kind.clone(), state_key.clone());
                        leaf_state.insert(key, leaf_pdu);
                    }

                    fork_states.insert(leaf_state);
                }
                _ => {
                    error!("Missing state snapshot for {:?}", id);
                    return Err("Missing state snapshot.".to_owned());
                }
            }
        }

        // 12. Ensure that the state is derived from the previous current state (i.e. we calculated
        //     by doing state res where one of the inputs was a previously trusted set of state,
        //     don't just trust a set of state we got from a remote).

        // We do this by adding the current state to the list of fork states
        let current_state = db
            .rooms
            .room_state_full(&room_id)
            .map_err(|_| "Failed to load room state.".to_owned())?;

        fork_states.insert(current_state.clone());

        // We also add state after incoming event to the fork states
        extremities.insert(incoming_pdu.event_id.clone());
        let mut state_after = state_at_incoming_event.clone();
        if let Some(state_key) = &incoming_pdu.state_key {
            state_after.insert(
                (incoming_pdu.kind.clone(), state_key.clone()),
                incoming_pdu.clone(),
            );
        }
        fork_states.insert(state_after.clone());

        let fork_states = fork_states.into_iter().collect::<Vec<_>>();

        let mut update_state = false;
        // 14. Use state resolution to find new room state
        let new_room_state = if fork_states.is_empty() {
            return Err("State is empty.".to_owned());
        } else if fork_states.len() == 1 {
            // There was only one state, so it has to be the room's current state (because that is
            // always included)
            debug!("Skipping stateres because there is no new state.");
            fork_states[0]
                .iter()
                .map(|(k, pdu)| (k.clone(), pdu.event_id.clone()))
                .collect()
        } else {
            // We do need to force an update to this room's state
            update_state = true;

            let mut auth_events = vec![];
            for map in &fork_states {
                let mut state_auth = vec![];
                for auth_id in map.values().flat_map(|pdu| &pdu.auth_events) {
                    match fetch_and_handle_events(&db, origin, &[auth_id.clone()], pub_key_map)
                        .await
                    {
                        // This should always contain exactly one element when Ok
                        Ok(events) => state_auth.extend_from_slice(&events),
                        Err(e) => {
                            debug!("Event was not present: {}", e);
                        }
                    }
                }
                auth_events.push(state_auth);
            }

            match state_res::StateResolution::resolve(
                &room_id,
                room_version_id,
                &fork_states
                    .into_iter()
                    .map(|map| {
                        map.into_iter()
                            .map(|(k, v)| (k, v.event_id.clone()))
                            .collect::<StateMap<_>>()
                    })
                    .collect::<Vec<_>>(),
                auth_events
                    .into_iter()
                    .map(|pdus| pdus.into_iter().map(|pdu| pdu.event_id().clone()).collect())
                    .collect(),
                &|id| {
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
            }
        };

        // 13. Check if the event passes auth based on the "current state" of the room, if not "soft fail" it
        let soft_fail = !state_res::event_auth::auth_check(
            &room_version,
            &incoming_pdu,
            previous_create,
            &current_state,
            None,
        )
        .map_err(|_e| "Auth check failed.".to_owned())?;

        let mut pdu_id = None;
        if !soft_fail {
            // Now that the event has passed all auth it is added into the timeline.
            // We use the `state_at_event` instead of `state_after` so we accurately
            // represent the state for this event.
            pdu_id = Some(
                append_incoming_pdu(
                    &db,
                    &incoming_pdu,
                    val,
                    extremities,
                    &state_at_incoming_event,
                )
                .map_err(|_| "Failed to add pdu to db.".to_owned())?,
            );
            debug!("Appended incoming pdu.");
        } else {
            warn!("Event was soft failed: {:?}", incoming_pdu);
        }

        // Set the new room state to the resolved state
        if update_state {
            db.rooms
                .force_state(&room_id, new_room_state, &db)
                .map_err(|_| "Failed to set new room state.".to_owned())?;
        }
        debug!("Updated resolved state");

        if soft_fail {
            // Soft fail, we leave the event as an outlier but don't add it to the timeline
            return Err("Event has been soft failed".into());
        }

        // Event has passed all auth/stateres checks
        Ok(pdu_id)
    })
}

/// Find the event and auth it. Once the event is validated (steps 1 - 8)
/// it is appended to the outliers Tree.
///
/// a. Look in the main timeline (pduid_pdu tree)
/// b. Look at outlier pdu tree
/// c. Ask origin server over federation
/// d. TODO: Ask other servers over federation?
//#[tracing::instrument(skip(db, key_map, auth_cache))]
pub(crate) fn fetch_and_handle_events<'a>(
    db: &'a Database,
    origin: &'a ServerName,
    events: &'a [EventId],
    pub_key_map: &'a RwLock<BTreeMap<String, BTreeMap<String, String>>>,
) -> AsyncRecursiveResult<'a, Vec<Arc<PduEvent>>, Error> {
    Box::pin(async move {
        let back_off = |id| match db.globals.bad_event_ratelimiter.write().unwrap().entry(id) {
            Entry::Vacant(e) => {
                e.insert((Instant::now(), 1));
            }
            Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
        };

        let mut pdus = vec![];
        for id in events {
            if let Some((time, tries)) = db.globals.bad_event_ratelimiter.read().unwrap().get(&id) {
                // Exponential backoff
                let mut min_elapsed_duration = Duration::from_secs(30) * (*tries) * (*tries);
                if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
                    min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
                }

                if time.elapsed() < min_elapsed_duration {
                    debug!("Backing off from {}", id);
                    continue;
                }
            }

            // a. Look in the main timeline (pduid_pdu tree)
            // b. Look at outlier pdu tree
            // (get_pdu checks both)
            let pdu = match db.rooms.get_pdu(&id)? {
                Some(pdu) => {
                    trace!("Found {} in db", id);
                    pdu
                }
                None => {
                    // c. Ask origin server over federation
                    debug!("Fetching {} over federation.", id);
                    match db
                        .sending
                        .send_federation_request(
                            &db.globals,
                            origin,
                            get_event::v1::Request { event_id: &id },
                        )
                        .await
                    {
                        Ok(res) => {
                            debug!("Got {} over federation", id);
                            let (event_id, mut value) =
                                crate::pdu::gen_event_id_canonical_json(&res.pdu)?;
                            // This will also fetch the auth chain
                            match handle_incoming_pdu(
                                origin,
                                &event_id,
                                value.clone(),
                                false,
                                db,
                                pub_key_map,
                            )
                            .await
                            {
                                Ok(_) => {
                                    value.insert(
                                        "event_id".to_owned(),
                                        CanonicalJsonValue::String(event_id.into()),
                                    );

                                    Arc::new(
                                        serde_json::from_value(
                                            serde_json::to_value(value)
                                                .expect("canonicaljsonobject is valid value"),
                                        )
                                        .expect(
                                            "This is possible because handle_incoming_pdu worked",
                                        ),
                                    )
                                }
                                Err(e) => {
                                    warn!("Authentication of event {} failed: {:?}", id, e);
                                    back_off(id.clone());
                                    continue;
                                }
                            }
                        }
                        Err(_) => {
                            warn!("Failed to fetch event: {}", id);
                            back_off(id.clone());
                            continue;
                        }
                    }
                }
            };
            pdus.push(pdu);
        }
        Ok(pdus)
    })
}

/// Search the DB for the signing keys of the given server, if we don't have them
/// fetch them from the server and save to our DB.
#[tracing::instrument(skip(db))]
pub(crate) async fn fetch_signing_keys(
    db: &Database,
    origin: &ServerName,
    signature_ids: Vec<String>,
) -> Result<BTreeMap<String, String>> {
    let contains_all_ids =
        |keys: &BTreeMap<String, String>| signature_ids.iter().all(|id| keys.contains_key(id));

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
        Entry::Vacant(e) => {
            e.insert((Instant::now(), 1));
        }
        Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
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

    let mut result = db
        .globals
        .signing_keys_for(origin)?
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.key))
        .collect::<BTreeMap<_, _>>();

    if contains_all_ids(&result) {
        return Ok(result);
    }

    debug!("Fetching signing keys for {} over federation", origin);

    if let Ok(get_keys_response) = db
        .sending
        .send_federation_request(&db.globals, origin, get_server_keys::v2::Request::new())
        .await
    {
        db.globals
            .add_signing_key(origin, get_keys_response.server_key.clone())?;

        result.extend(
            get_keys_response
                .server_key
                .verify_keys
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.key)),
        );
        result.extend(
            get_keys_response
                .server_key
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
        if let Ok(keys) = db
            .sending
            .send_federation_request(
                &db.globals,
                &server,
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
        {
            trace!("Got signing keys: {:?}", keys);
            for k in keys.server_keys {
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
#[tracing::instrument(skip(db))]
pub(crate) fn append_incoming_pdu(
    db: &Database,
    pdu: &PduEvent,
    pdu_json: CanonicalJsonObject,
    new_room_leaves: HashSet<EventId>,
    state: &StateMap<Arc<PduEvent>>,
) -> Result<Vec<u8>> {
    let count = db.globals.next_count()?;
    let mut pdu_id = pdu.room_id.as_bytes().to_vec();
    pdu_id.push(0xff);
    pdu_id.extend_from_slice(&count.to_be_bytes());

    // We append to state before appending the pdu, so we don't have a moment in time with the
    // pdu without it's state. This is okay because append_pdu can't fail.
    db.rooms
        .set_event_state(&pdu.event_id, state, &db.globals)?;

    db.rooms.append_pdu(
        pdu,
        pdu_json,
        count,
        &pdu_id,
        &new_room_leaves.into_iter().collect::<Vec<_>>(),
        &db,
    )?;

    for appservice in db.appservice.iter_all()?.filter_map(|r| r.ok()) {
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
                .and_then(|users| users.get("regex"))
                .and_then(|regex| regex.as_str())
                .and_then(|regex| Regex::new(regex).ok());
            let rooms = namespaces
                .get("rooms")
                .and_then(|rooms| rooms.as_sequence());

            let room_aliases = db.rooms.room_aliases(&pdu.room_id);

            let bridge_user_id = appservice
                .1
                .get("sender_localpart")
                .and_then(|string| string.as_str())
                .and_then(|string| {
                    UserId::parse_with_server_name(string, db.globals.server_name()).ok()
                });

            #[allow(clippy::blocks_in_if_conditions)]
            if bridge_user_id.map_or(false, |bridge_user_id| {
                db.rooms
                    .is_joined(&bridge_user_id, &pdu.room_id)
                    .unwrap_or(false)
            }) || users.iter().any(|users| {
                users.is_match(pdu.sender.as_str())
                    || pdu.kind == EventType::RoomMember
                        && pdu
                            .state_key
                            .as_ref()
                            .map_or(false, |state_key| users.is_match(&state_key))
            }) || aliases.map_or(false, |aliases| {
                room_aliases
                    .filter_map(|r| r.ok())
                    .any(|room_alias| aliases.is_match(room_alias.as_str()))
            }) || rooms.map_or(false, |rooms| rooms.contains(&pdu.room_id.as_str().into()))
                || db
                    .rooms
                    .room_members(&pdu.room_id)
                    .filter_map(|r| r.ok())
                    .any(|member| users.iter().any(|regex| regex.is_match(member.as_str())))
            {
                db.sending.send_pdu_appservice(&appservice.0, &pdu_id)?;
            }
        }
    }

    Ok(pdu_id)
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/event/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn get_event_route(
    db: DatabaseGuard,
    body: Ruma<get_event::v1::Request<'_>>,
) -> ConduitResult<get_event::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    Ok(get_event::v1::Response {
        origin: db.globals.server_name().to_owned(),
        origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
        pdu: PduEvent::convert_to_outgoing_federation_event(
            db.rooms
                .get_pdu_json(&body.event_id)?
                .ok_or(Error::BadRequest(ErrorKind::NotFound, "Event not found."))?,
        ),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/federation/v1/get_missing_events/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn get_missing_events_route(
    db: DatabaseGuard,
    body: Ruma<get_missing_events::v1::Request<'_>>,
) -> ConduitResult<get_missing_events::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut queued_events = body.latest_events.clone();
    let mut events = Vec::new();

    let mut i = 0;
    while i < queued_events.len() && events.len() < u64::from(body.limit) as usize {
        if let Some(pdu) = db.rooms.get_pdu_json(&queued_events[i])? {
            let event_id =
                serde_json::from_value(
                    serde_json::to_value(pdu.get("event_id").cloned().ok_or_else(|| {
                        Error::bad_database("Event in db has no event_id field.")
                    })?)
                    .expect("canonical json is valid json value"),
                )
                .map_err(|_| Error::bad_database("Invalid event_id field in pdu in db."))?;

            if body.earliest_events.contains(&event_id) {
                i += 1;
                continue;
            }
            queued_events.extend_from_slice(
                &serde_json::from_value::<Vec<EventId>>(
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

    Ok(get_missing_events::v1::Response { events }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/event_auth/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn get_event_authorization_route(
    db: DatabaseGuard,
    body: Ruma<get_event_authorization::v1::Request<'_>>,
) -> ConduitResult<get_event_authorization::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut auth_chain = Vec::new();
    let mut auth_chain_ids = BTreeSet::<EventId>::new();
    let mut todo = BTreeSet::new();
    todo.insert(body.event_id.clone());

    while let Some(event_id) = todo.iter().next().cloned() {
        if let Some(pdu) = db.rooms.get_pdu(&event_id)? {
            todo.extend(
                pdu.auth_events
                    .clone()
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .difference(&auth_chain_ids)
                    .cloned(),
            );
            auth_chain_ids.extend(pdu.auth_events.clone().into_iter());

            let pdu_json = PduEvent::convert_to_outgoing_federation_event(
                db.rooms.get_pdu_json(&event_id)?.unwrap(),
            );
            auth_chain.push(pdu_json);
        } else {
            warn!("Could not find pdu mentioned in auth events.");
        }

        todo.remove(&event_id);
    }

    Ok(get_event_authorization::v1::Response { auth_chain }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/state/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn get_room_state_route(
    db: DatabaseGuard,
    body: Ruma<get_room_state::v1::Request<'_>>,
) -> ConduitResult<get_room_state::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

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
        .map(|id| {
            PduEvent::convert_to_outgoing_federation_event(
                db.rooms.get_pdu_json(&id).unwrap().unwrap(),
            )
        })
        .collect();

    let mut auth_chain = Vec::new();
    let mut auth_chain_ids = BTreeSet::<EventId>::new();
    let mut todo = BTreeSet::new();
    todo.insert(body.event_id.clone());

    while let Some(event_id) = todo.iter().next().cloned() {
        if let Some(pdu) = db.rooms.get_pdu(&event_id)? {
            todo.extend(
                pdu.auth_events
                    .clone()
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .difference(&auth_chain_ids)
                    .cloned(),
            );
            auth_chain_ids.extend(pdu.auth_events.clone().into_iter());

            let pdu_json = PduEvent::convert_to_outgoing_federation_event(
                db.rooms.get_pdu_json(&event_id)?.unwrap(),
            );
            auth_chain.push(pdu_json);
        } else {
            warn!("Could not find pdu mentioned in auth events.");
        }

        todo.remove(&event_id);
    }

    Ok(get_room_state::v1::Response { auth_chain, pdus }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/state_ids/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn get_room_state_ids_route(
    db: DatabaseGuard,
    body: Ruma<get_room_state_ids::v1::Request<'_>>,
) -> ConduitResult<get_room_state_ids::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let shortstatehash = db
        .rooms
        .pdu_shortstatehash(&body.event_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Pdu state not found.",
        ))?;

    let pdu_ids = db.rooms.state_full_ids(shortstatehash)?;

    let mut auth_chain_ids = BTreeSet::<EventId>::new();
    let mut todo = BTreeSet::new();
    todo.insert(body.event_id.clone());

    while let Some(event_id) = todo.iter().next().cloned() {
        if let Some(pdu) = db.rooms.get_pdu(&event_id)? {
            todo.extend(
                pdu.auth_events
                    .clone()
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .difference(&auth_chain_ids)
                    .cloned(),
            );
            auth_chain_ids.extend(pdu.auth_events.clone().into_iter());
        } else {
            warn!("Could not find pdu mentioned in auth events.");
        }

        todo.remove(&event_id);
    }

    Ok(get_room_state_ids::v1::Response {
        auth_chain_ids: auth_chain_ids.into_iter().collect(),
        pdu_ids,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/make_join/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn create_join_event_template_route(
    db: DatabaseGuard,
    body: Ruma<create_join_event_template::v1::Request<'_>>,
) -> ConduitResult<create_join_event_template::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    if !db.rooms.exists(&body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Server is not in room.",
        ));
    }

    if !body.ver.contains(&RoomVersionId::Version6) {
        return Err(Error::BadRequest(
            ErrorKind::IncompatibleRoomVersion {
                room_version: RoomVersionId::Version6,
            },
            "Room version not supported.",
        ));
    }

    let prev_events = db
        .rooms
        .get_pdu_leaves(&body.room_id)?
        .into_iter()
        .take(20)
        .collect::<Vec<_>>();

    let create_event = db
        .rooms
        .room_state_get(&body.room_id, &EventType::RoomCreate, "")?;

    let create_event_content = create_event
        .as_ref()
        .map(|create_event| {
            serde_json::from_value::<Raw<CreateEventContent>>(create_event.content.clone())
                .expect("Raw::from_value always works.")
                .deserialize()
                .map_err(|_| Error::bad_database("Invalid PowerLevels event in db."))
        })
        .transpose()?;

    let create_prev_event = if prev_events.len() == 1
        && Some(&prev_events[0]) == create_event.as_ref().map(|c| &c.event_id)
    {
        create_event
    } else {
        None
    };

    // If there was no create event yet, assume we are creating a version 6 room right now
    let room_version = RoomVersion::new(
        &create_event_content.map_or(RoomVersionId::Version6, |create_event| {
            create_event.room_version
        }),
    )
    .expect("room version is supported");

    let content = serde_json::to_value(MemberEventContent {
        avatar_url: None,
        displayname: None,
        is_direct: None,
        membership: MembershipState::Join,
        third_party_invite: None,
    })
    .expect("member event is valid value");

    let state_key = body.user_id.to_string();
    let kind = EventType::RoomMember;

    let auth_events = db.rooms.get_auth_events(
        &body.room_id,
        &kind,
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
            serde_json::to_value(&prev_pdu.sender).expect("UserId::to_value always works"),
        );
    }

    let pdu = PduEvent {
        event_id: ruma::event_id!("$thiswillbefilledinlater"),
        room_id: body.room_id.clone(),
        sender: body.user_id.clone(),
        origin_server_ts: utils::millis_since_unix_epoch()
            .try_into()
            .expect("time is valid"),
        kind,
        content,
        state_key: Some(state_key),
        prev_events,
        depth,
        auth_events: auth_events
            .iter()
            .map(|(_, pdu)| pdu.event_id.clone())
            .collect(),
        redacts: None,
        unsigned,
        hashes: ruma::events::pdu::EventHash {
            sha256: "aaa".to_owned(),
        },
        signatures: BTreeMap::new(),
    };

    let auth_check = state_res::auth_check(
        &room_version,
        &Arc::new(pdu.clone()),
        create_prev_event,
        &auth_events,
        None, // TODO: third_party_invite
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

    Ok(create_join_event_template::v1::Response {
        room_version: Some(RoomVersionId::Version6),
        event: serde_json::from_value::<Raw<_>>(
            serde_json::to_value(pdu_json).expect("CanonicalJson is valid serde_json::Value"),
        )
        .expect("Raw::from_value always works"),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/federation/v2/send_join/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn create_join_event_route(
    db: DatabaseGuard,
    body: Ruma<create_join_event::v2::Request<'_>>,
) -> ConduitResult<create_join_event::v2::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    // We need to return the state prior to joining, let's keep a reference to that here
    let shortstatehash =
        db.rooms
            .current_shortstatehash(&body.room_id)?
            .ok_or(Error::BadRequest(
                ErrorKind::NotFound,
                "Pdu state not found.",
            ))?;

    let pub_key_map = RwLock::new(BTreeMap::new());
    // let mut auth_cache = EventMap::new();

    // We do not add the event_id field to the pdu here because of signature and hashes checks
    let (event_id, value) = match crate::pdu::gen_event_id_canonical_json(&body.pdu) {
        Ok(t) => t,
        Err(_) => {
            // Event could not be converted to canonical json
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Could not convert event to canonical json.",
            ));
        }
    };

    let origin = serde_json::from_value::<Box<ServerName>>(
        serde_json::to_value(value.get("origin").ok_or(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Event needs an origin field.",
        ))?)
        .expect("CanonicalJson is valid json value"),
    )
    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Origin field is invalid."))?;

    let pdu_id = handle_incoming_pdu(&origin, &event_id, value, true, &db, &pub_key_map)
        .await
        .map_err(|_| {
            Error::BadRequest(
                ErrorKind::InvalidParam,
                "Error while handling incoming PDU.",
            )
        })?
        .ok_or(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Could not accept incoming PDU as timeline event.",
        ))?;

    let state_ids = db.rooms.state_full_ids(shortstatehash)?;

    let mut auth_chain_ids = BTreeSet::<EventId>::new();
    let mut todo = state_ids.iter().cloned().collect::<BTreeSet<_>>();

    while let Some(event_id) = todo.iter().next().cloned() {
        if let Some(pdu) = db.rooms.get_pdu(&event_id)? {
            todo.extend(
                pdu.auth_events
                    .clone()
                    .into_iter()
                    .collect::<BTreeSet<_>>()
                    .difference(&auth_chain_ids)
                    .cloned(),
            );
            auth_chain_ids.extend(pdu.auth_events.clone().into_iter());
        } else {
            warn!("Could not find pdu mentioned in auth events.");
        }

        todo.remove(&event_id);
    }

    for server in db
        .rooms
        .room_servers(&body.room_id)
        .filter_map(|r| r.ok())
        .filter(|server| &**server != db.globals.server_name())
    {
        db.sending.send_pdu(&server, &pdu_id)?;
    }

    db.flush().await?;

    Ok(create_join_event::v2::Response {
        room_state: RoomState {
            auth_chain: auth_chain_ids
                .iter()
                .filter_map(|id| db.rooms.get_pdu_json(&id).ok().flatten())
                .map(PduEvent::convert_to_outgoing_federation_event)
                .collect(),
            state: state_ids
                .iter()
                .filter_map(|id| db.rooms.get_pdu_json(&id).ok().flatten())
                .map(PduEvent::convert_to_outgoing_federation_event)
                .collect(),
        },
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/federation/v2/invite/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn create_invite_route(
    db: DatabaseGuard,
    body: Ruma<create_invite::v2::Request>,
) -> ConduitResult<create_invite::v2::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    if body.room_version < RoomVersionId::Version6 {
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
    let event_id = EventId::try_from(&*format!(
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

    let sender = serde_json::from_value(
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

    let invited_user = serde_json::from_value(
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

    let mut event = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(
        &body.event.json().to_string(),
    )
    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid invite event bytes."))?;

    event.insert("event_id".to_owned(), "$dummy".into());

    let pdu = serde_json::from_value::<PduEvent>(event.into()).map_err(|e| {
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
        )?;
    }

    db.flush().await?;

    Ok(create_invite::v2::Response {
        event: PduEvent::convert_to_outgoing_federation_event(signed_event),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/user/devices/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn get_devices_route(
    db: DatabaseGuard,
    body: Ruma<get_devices::v1::Request<'_>>,
) -> ConduitResult<get_devices::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

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
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/query/directory", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn get_room_information_route(
    db: DatabaseGuard,
    body: Ruma<get_room_information::v1::Request<'_>>,
) -> ConduitResult<get_room_information::v1::Response> {
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
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/federation/v1/query/profile", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub fn get_profile_information_route(
    db: DatabaseGuard,
    body: Ruma<get_profile_information::v1::Request<'_>>,
) -> ConduitResult<get_profile_information::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let mut displayname = None;
    let mut avatar_url = None;

    match &body.field {
        // TODO: what to do with custom
        Some(ProfileField::_Custom(_s)) => {}
        Some(ProfileField::DisplayName) => displayname = db.users.displayname(&body.user_id)?,
        Some(ProfileField::AvatarUrl) => avatar_url = db.users.avatar_url(&body.user_id)?,
        None => {
            displayname = db.users.displayname(&body.user_id)?;
            avatar_url = db.users.avatar_url(&body.user_id)?;
        }
    }

    Ok(get_profile_information::v1::Response {
        displayname,
        avatar_url,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/federation/v1/user/keys/query", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_keys_route(
    db: DatabaseGuard,
    body: Ruma<get_keys::v1::Request>,
) -> ConduitResult<get_keys::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let result = get_keys_helper(
        None,
        &body.device_keys,
        |u| Some(u.server_name()) == body.sender_servername.as_deref(),
        &db,
    )?;

    db.flush().await?;

    Ok(get_keys::v1::Response {
        device_keys: result.device_keys,
        master_keys: result.master_keys,
        self_signing_keys: result.self_signing_keys,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/federation/v1/user/keys/claim", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn claim_keys_route(
    db: DatabaseGuard,
    body: Ruma<claim_keys::v1::Request>,
) -> ConduitResult<claim_keys::v1::Response> {
    if !db.globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    let result = claim_keys_helper(&body.one_time_keys, &db)?;

    db.flush().await?;

    Ok(claim_keys::v1::Response {
        one_time_keys: result.one_time_keys,
    }
    .into())
}

pub async fn fetch_required_signing_keys(
    event: &BTreeMap<String, CanonicalJsonValue>,
    pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, String>>>,
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
            &Box::<ServerName>::try_from(&**signature_server).map_err(|_| {
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
