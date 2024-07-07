#![allow(deprecated)]

use crate::{
    api::client_server::{self, claim_keys_helper, get_keys_helper},
    service::{
        globals::SigningKeys,
        pdu::{gen_event_id_canonical_json, PduBuilder},
    },
    services, utils, Error, PduEvent, Result, Ruma,
};
use axum::{response::IntoResponse, Json};
use axum_extra::headers::{authorization::Credentials, CacheControl, Header};
use get_profile_information::v1::ProfileField;
use http::header::AUTHORIZATION;

use ruma::{
    api::{
        client::error::{Error as RumaError, ErrorKind},
        federation::{
            authorization::get_event_authorization,
            backfill::get_backfill,
            device::get_devices::{self, v1::UserDevice},
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                discover_homeserver, get_server_keys, get_server_version, ServerSigningKeys,
                VerifyKey,
            },
            event::{get_event, get_missing_events, get_room_state, get_room_state_ids},
            keys::{claim_keys, get_keys},
            membership::{create_invite, create_join_event, prepare_join_event},
            openid::get_openid_userinfo,
            query::{get_profile_information, get_room_information},
            transactions::{
                edu::{DeviceListUpdateContent, DirectDeviceContent, Edu, SigningKeyUpdateContent},
                send_transaction_message,
            },
        },
        EndpointError, IncomingResponse, MatrixVersion, OutgoingRequest, OutgoingResponse,
        SendAccessToken,
    },
    directory::{Filter, RoomNetwork},
    events::{
        receipt::{ReceiptEvent, ReceiptEventContent, ReceiptType},
        room::{
            join_rules::{JoinRule, RoomJoinRulesEventContent},
            member::{MembershipState, RoomMemberEventContent},
        },
        StateEventType, TimelineEventType,
    },
    serde::{Base64, JsonObject, Raw},
    server_util::authorization::XMatrix,
    to_device::DeviceIdOrAllDevices,
    uint, user_id, CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch,
    OwnedEventId, OwnedRoomId, OwnedServerName, OwnedServerSigningKeyId, OwnedUserId, RoomId,
    ServerName,
};
use serde_json::value::{to_raw_value, RawValue as RawJsonValue};
use std::{
    collections::BTreeMap,
    fmt::Debug,
    mem,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};
use tokio::sync::RwLock;

use tracing::{debug, error, warn};

/// Wraps either an literal IP address plus port, or a hostname plus complement
/// (colon-plus-port if it was specified).
///
/// Note: A `FedDest::Named` might contain an IP address in string form if there
/// was no port specified to construct a SocketAddr with.
///
/// # Examples:
/// ```rust
/// # use conduit::api::server_server::FedDest;
/// # fn main() -> Result<(), std::net::AddrParseError> {
/// FedDest::Literal("198.51.100.3:8448".parse()?);
/// FedDest::Literal("[2001:db8::4:5]:443".parse()?);
/// FedDest::Named("matrix.example.org".to_owned(), "".to_owned());
/// FedDest::Named("matrix.example.org".to_owned(), ":8448".to_owned());
/// FedDest::Named("198.51.100.5".to_owned(), "".to_owned());
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FedDest {
    Literal(SocketAddr),
    Named(String, String),
}

impl FedDest {
    fn into_https_string(self) -> String {
        match self {
            Self::Literal(addr) => format!("https://{addr}"),
            Self::Named(host, port) => format!("https://{host}{port}"),
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
pub(crate) async fn send_request<T>(
    destination: &ServerName,
    request: T,
) -> Result<T::IncomingResponse>
where
    T: OutgoingRequest + Debug,
{
    if !services().globals.allow_federation() {
        return Err(Error::bad_config("Federation is disabled."));
    }

    if destination == services().globals.server_name() {
        return Err(Error::bad_config(
            "Won't send federation request to ourselves",
        ));
    }

    debug!("Preparing to send request to {destination}");

    let cached_result = services()
        .globals
        .actual_destination_cache
        .read()
        .await
        .get(destination)
        .cloned();

    let actual_destination = if let Some(DestinationResponse {
        actual_destination,
        dest_type,
    }) = cached_result
    {
        match dest_type {
            DestType::IsIpOrHasPort => actual_destination,
            DestType::LookupFailed {
                well_known_retry,
                well_known_backoff_mins,
            } => {
                if well_known_retry < Instant::now() {
                    find_actual_destination(destination, None, false, Some(well_known_backoff_mins))
                        .await
                } else {
                    actual_destination
                }
            }

            DestType::WellKnown { expires } => {
                if expires < Instant::now() {
                    find_actual_destination(destination, None, false, None).await
                } else {
                    actual_destination
                }
            }
            DestType::WellKnownSrv {
                srv_expires,
                well_known_expires,
                well_known_host,
            } => {
                if well_known_expires < Instant::now() {
                    find_actual_destination(destination, None, false, None).await
                } else if srv_expires < Instant::now() {
                    find_actual_destination(destination, Some(well_known_host), true, None).await
                } else {
                    actual_destination
                }
            }
            DestType::Srv {
                well_known_retry,
                well_known_backoff_mins,
                srv_expires,
            } => {
                if well_known_retry < Instant::now() {
                    find_actual_destination(destination, None, false, Some(well_known_backoff_mins))
                        .await
                } else if srv_expires < Instant::now() {
                    find_actual_destination(destination, None, true, Some(well_known_backoff_mins))
                        .await
                } else {
                    actual_destination
                }
            }
        }
    } else {
        find_actual_destination(destination, None, false, None).await
    };

    let actual_destination_str = actual_destination.clone().into_https_string();

    let mut http_request = request
        .try_into_http_request::<Vec<u8>>(
            &actual_destination_str,
            SendAccessToken::IfRequired(""),
            &[MatrixVersion::V1_4],
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
                XMatrix::parse(&format!(
                    "X-Matrix origin=\"{}\",destination=\"{}\",key=\"{}\",sig=\"{}\"",
                    services().globals.server_name(),
                    destination,
                    s.0,
                    s.1
                ))
                .expect("When Ruma signs JSON, it produces a valid base64 signature. All other types are valid ServerNames or OwnedKeyId")
                .encode(),
            );
        }
    }

    let reqwest_request = reqwest::Request::try_from(http_request)?;

    let url = reqwest_request.url().clone();

    debug!("Sending request to {destination} at {url}");
    let response = services()
        .globals
        .federation_client()
        .execute(reqwest_request)
        .await;
    debug!("Received response from {destination} at {url}");

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

            debug!("Getting response bytes from {destination}");
            let body = response.bytes().await.unwrap_or_else(|e| {
                warn!("server error {}", e);
                Vec::new().into()
            }); // TODO: handle timeout
            debug!("Got response bytes from {destination}");

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
                debug!("Parsing response bytes from {destination}");
                let response = T::IncomingResponse::try_from_http_response(http_response);

                response.map_err(|e| {
                    warn!(
                        "Invalid 200 response from {} on: {} {}",
                        &destination, url, e
                    );
                    Error::BadServerResponse("Server returned bad 200 response.")
                })
            } else {
                debug!("Returning error from {destination}");
                Err(Error::FederationError(
                    destination.to_owned(),
                    RumaError::from_http_response(http_response),
                ))
            }
        }
        Err(e) => {
            warn!(
                "Could not send request to {} at {}: {}",
                destination, actual_destination_str, e
            );
            Err(e.into())
        }
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

#[derive(Clone)]
pub struct DestinationResponse {
    pub actual_destination: FedDest,
    pub dest_type: DestType,
}

#[derive(Clone)]
pub enum DestType {
    WellKnownSrv {
        srv_expires: Instant,
        well_known_expires: Instant,
        well_known_host: String,
    },
    WellKnown {
        expires: Instant,
    },
    Srv {
        srv_expires: Instant,
        well_known_retry: Instant,
        well_known_backoff_mins: u16,
    },
    IsIpOrHasPort,
    LookupFailed {
        well_known_retry: Instant,
        well_known_backoff_mins: u16,
    },
}

/// Implemented according to the specification at <https://spec.matrix.org/v1.11/server-server-api/#resolving-server-names>
/// Numbers in comments below refer to bullet points in linked section of specification
async fn find_actual_destination(
    destination: &'_ ServerName,
    // The host used to potentially lookup SRV records against, only used when only_request_srv is true
    well_known_dest: Option<String>,
    // Should be used when only the SRV lookup has expired
    only_request_srv: bool,
    // The backoff time for the last well known failure, if any
    well_known_backoff_mins: Option<u16>,
) -> FedDest {
    debug!("Finding actual destination for {destination}");
    let destination_str = destination.to_string();
    let next_backoff_mins = well_known_backoff_mins
        // Errors are recommended to be cached for up to an hour
        .map(|mins| (mins * 2).min(60))
        .unwrap_or(1);

    let (actual_destination, dest_type) = if only_request_srv {
        let destination_str = well_known_dest.unwrap_or(destination_str);
        let (dest, expires) = get_srv_destination(destination_str).await;
        let well_known_retry =
            Instant::now() + Duration::from_secs((60 * next_backoff_mins).into());
        (
            dest,
            if let Some(expires) = expires {
                DestType::Srv {
                    well_known_backoff_mins: next_backoff_mins,
                    srv_expires: expires,

                    well_known_retry,
                }
            } else {
                DestType::LookupFailed {
                    well_known_retry,
                    well_known_backoff_mins: next_backoff_mins,
                }
            },
        )
    } else {
        match get_ip_with_port(&destination_str) {
            Some(host_port) => {
                debug!("1: IP literal with provided or default port");
                (host_port, DestType::IsIpOrHasPort)
            }
            None => {
                if let Some(pos) = destination_str.find(':') {
                    debug!("2: Hostname with included port");
                    let (host, port) = destination_str.split_at(pos);
                    (
                        FedDest::Named(host.to_owned(), port.to_owned()),
                        DestType::IsIpOrHasPort,
                    )
                } else {
                    debug!("Requesting well known for {destination_str}");
                    match request_well_known(destination_str.as_str()).await {
                        Some((delegated_hostname, timestamp)) => {
                            debug!("3: A .well-known file is available");
                            match get_ip_with_port(&delegated_hostname) {
                                // 3.1: IP literal in .well-known file
                                Some(host_and_port) => {
                                    (host_and_port, DestType::WellKnown { expires: timestamp })
                                }
                                None => {
                                    if let Some(pos) = delegated_hostname.find(':') {
                                        debug!("3.2: Hostname with port in .well-known file");
                                        let (host, port) = delegated_hostname.split_at(pos);
                                        (
                                            FedDest::Named(host.to_owned(), port.to_owned()),
                                            DestType::WellKnown { expires: timestamp },
                                        )
                                    } else {
                                        debug!("Delegated hostname has no port in this branch");
                                        let (dest, srv_expires) =
                                            get_srv_destination(delegated_hostname.clone()).await;
                                        (
                                            dest,
                                            if let Some(srv_expires) = srv_expires {
                                                DestType::WellKnownSrv {
                                                    srv_expires,
                                                    well_known_expires: timestamp,
                                                    well_known_host: delegated_hostname,
                                                }
                                            } else {
                                                DestType::WellKnown { expires: timestamp }
                                            },
                                        )
                                    }
                                }
                            }
                        }
                        None => {
                            debug!("4: No .well-known or an error occured");
                            let (dest, expires) = get_srv_destination(destination_str).await;
                            let well_known_retry = Instant::now()
                                + Duration::from_secs((60 * next_backoff_mins).into());
                            (
                                dest,
                                if let Some(expires) = expires {
                                    DestType::Srv {
                                        srv_expires: expires,
                                        well_known_retry,
                                        well_known_backoff_mins: next_backoff_mins,
                                    }
                                } else {
                                    DestType::LookupFailed {
                                        well_known_retry,
                                        well_known_backoff_mins: next_backoff_mins,
                                    }
                                },
                            )
                        }
                    }
                }
            }
        }
    };

    debug!("Actual destination: {actual_destination:?}");

    let response = DestinationResponse {
        actual_destination,
        dest_type,
    };

    services()
        .globals
        .actual_destination_cache
        .write()
        .await
        .insert(destination.to_owned(), response.clone());

    response.actual_destination
}

/// Looks up the SRV records for federation usage
///
/// If no timestamp is returned, that means no SRV record was found
async fn get_srv_destination(delegated_hostname: String) -> (FedDest, Option<Instant>) {
    if let Some((hostname_override, timestamp)) = query_srv_record(&delegated_hostname).await {
        debug!("SRV lookup successful");
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
                    (override_ip.iter().collect(), force_port.unwrap_or(8448)),
                );
        } else {
            // Removing in case there was previously a SRV record
            services()
                .globals
                .tls_name_override
                .write()
                .unwrap()
                .remove(&delegated_hostname);
            warn!("Using SRV record, but could not resolve to IP");
        }

        if let Some(port) = force_port {
            (
                FedDest::Named(delegated_hostname, format!(":{port}")),
                Some(timestamp),
            )
        } else {
            (add_port_to_hostname(&delegated_hostname), Some(timestamp))
        }
    } else {
        debug!("No SRV records found");
        (add_port_to_hostname(&delegated_hostname), None)
    }
}

async fn query_given_srv_record(record: &str) -> Option<(FedDest, Instant)> {
    services()
        .globals
        .dns_resolver()
        .srv_lookup(record)
        .await
        .map(|srv| {
            srv.iter().next().map(|result| {
                (
                    FedDest::Named(
                        result.target().to_string().trim_end_matches('.').to_owned(),
                        format!(":{}", result.port()),
                    ),
                    srv.as_lookup().valid_until(),
                )
            })
        })
        .unwrap_or(None)
}

async fn query_srv_record(hostname: &'_ str) -> Option<(FedDest, Instant)> {
    let hostname = hostname.trim_end_matches('.');

    if let Some(host_port) = query_given_srv_record(&format!("_matrix-fed._tcp.{hostname}.")).await
    {
        Some(host_port)
    } else {
        query_given_srv_record(&format!("_matrix._tcp.{hostname}.")).await
    }
}

async fn request_well_known(destination: &str) -> Option<(String, Instant)> {
    let response = services()
        .globals
        .default_client()
        .get(&format!("https://{destination}/.well-known/matrix/server"))
        .send()
        .await;
    debug!("Got well known response");
    let response = match response {
        Err(e) => {
            debug!("Well known error: {e:?}");
            return None;
        }
        Ok(r) => r,
    };

    let mut headers = response.headers().values();

    let cache_for = CacheControl::decode(&mut headers)
        .ok()
        .and_then(|cc| {
            // Servers should respect the cache control headers present on the response, or use a sensible default when headers are not present.
            if cc.no_store() || cc.no_cache() {
                Some(Duration::ZERO)
            } else {
                cc.max_age()
                    // Servers should additionally impose a maximum cache time for responses: 48 hours is recommended.
                    .map(|age| age.min(Duration::from_secs(60 * 60 * 48)))
            }
        })
        // The recommended sensible default is 24 hours.
        .unwrap_or_else(|| Duration::from_secs(60 * 60 * 24));

    let text = response.text().await;
    debug!("Got well known response text");

    let host = || {
        let body: serde_json::Value = serde_json::from_str(&text.ok()?).ok()?;
        body.get("m.server")?.as_str().map(ToOwned::to_owned)
    };

    host().map(|host| (host, Instant::now() + cache_for))
}

/// # `GET /_matrix/federation/v1/version`
///
/// Get version information on this server.
pub async fn get_server_version_route(
    _body: Ruma<get_server_version::v1::Request>,
) -> Result<get_server_version::v1::Response> {
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
    let mut verify_keys: BTreeMap<OwnedServerSigningKeyId, VerifyKey> = BTreeMap::new();
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
    body: Ruma<get_public_rooms_filtered::v1::Request>,
) -> Result<get_public_rooms_filtered::v1::Response> {
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
    body: Ruma<get_public_rooms::v1::Request>,
) -> Result<get_public_rooms::v1::Response> {
    let response = client_server::get_public_rooms_filtered_helper(
        None,
        body.limit,
        body.since.as_deref(),
        &Filter::default(),
        &RoomNetwork::Matrix,
    )
    .await?;

    Ok(get_public_rooms::v1::Response {
        chunk: response.chunk,
        prev_batch: response.prev_batch,
        next_batch: response.next_batch,
        total_room_count_estimate: response.total_room_count_estimate,
    })
}

pub fn parse_incoming_pdu(
    pdu: &RawJsonValue,
) -> Result<(OwnedEventId, CanonicalJsonObject, OwnedRoomId)> {
    let value: CanonicalJsonObject = serde_json::from_str(pdu.get()).map_err(|e| {
        warn!("Error parsing incoming event {:?}: {:?}", pdu, e);
        Error::BadServerResponse("Invalid PDU in server response")
    })?;

    let room_id: OwnedRoomId = value
        .get("room_id")
        .and_then(|id| RoomId::parse(id.as_str()?).ok())
        .ok_or(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Invalid room id in pdu",
        ))?;

    let room_version_id = services().rooms.state.get_room_version(&room_id)?;

    let (event_id, value) = match gen_event_id_canonical_json(pdu, &room_version_id) {
        Ok(t) => t,
        Err(e) => {
            // Event could not be converted to canonical json
            return Err(e);
        }
    };
    Ok((event_id, value, room_id))
}

/// Attempts to parse and append PDU to timeline.
/// If no event ID is returned, then the PDU was failed to be parsed.
/// If the Ok(()) is returned, then the PDU was successfully appended to the timeline.
async fn handle_pdu_in_transaction(
    origin: &ServerName,
    pub_key_map: &RwLock<BTreeMap<String, SigningKeys>>,
    pdu: &RawJsonValue,
) -> (Option<OwnedEventId>, Result<()>) {
    let (event_id, value, room_id) = match parse_incoming_pdu(pdu) {
        Ok(t) => t,
        Err(e) => {
            warn!("Could not parse PDU: {e}");
            warn!("Full PDU: {:?}", &pdu);
            return (None, Err(Error::BadServerResponse("Could not parse PDU")));
        }
    };

    // Makes use of the m.room.create event. If we cannot fetch this event,
    // we must have never been in that room.
    if services().rooms.state.get_room_version(&room_id).is_err() {
        debug!("Room {room_id} is not known to this server");
        return (
            Some(event_id),
            Err(Error::BadServerResponse("Room is not known to this server")),
        );
    }

    // We do not add the event_id field to the pdu here because of signature and hashes checks

    let mutex = Arc::clone(
        services()
            .globals
            .roomid_mutex_federation
            .write()
            .await
            .entry(room_id.to_owned())
            .or_default(),
    );
    let mutex_lock = mutex.lock().await;
    let start_time = Instant::now();

    if let Err(e) = services()
        .rooms
        .event_handler
        .handle_incoming_pdu(origin, &event_id, &room_id, value, true, pub_key_map)
        .await
    {
        warn!("Error appending PDU to timeline: {}: {:?}", e, pdu);
        return (Some(event_id), Err(e));
    }

    drop(mutex_lock);

    let elapsed = start_time.elapsed();
    debug!(
        "Handling transaction of event {} took {}m{}s",
        event_id,
        elapsed.as_secs() / 60,
        elapsed.as_secs() % 60
    );

    (Some(event_id), Ok(()))
}

/// # `PUT /_matrix/federation/v1/send/{txnId}`
///
/// Push EDUs and PDUs to this server.
pub async fn send_transaction_message_route(
    body: Ruma<send_transaction_message::v1::Request>,
) -> Result<send_transaction_message::v1::Response> {
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
        let (event_id, result) =
            handle_pdu_in_transaction(sender_servername, &pub_key_map, pdu).await;

        if let Some(event_id) = event_id {
            resolved_map.insert(event_id.clone(), result.map_err(|e| e.sanitized_error()));
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
                        if user_id.server_name() == sender_servername {
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
                                debug!("No known event ids in read receipt: {:?}", user_updates);
                            }
                        }
                    }
                }
            }
            Edu::Typing(typing) => {
                if typing.user_id.server_name() == sender_servername
                    && services()
                        .rooms
                        .state_cache
                        .is_joined(&typing.user_id, &typing.room_id)?
                {
                    if typing.typing {
                        services()
                            .rooms
                            .edus
                            .typing
                            .typing_add(
                                &typing.user_id,
                                &typing.room_id,
                                3000 + utils::millis_since_unix_epoch(),
                            )
                            .await?;
                    } else {
                        services()
                            .rooms
                            .edus
                            .typing
                            .typing_remove(&typing.user_id, &typing.room_id)
                            .await?;
                    }
                }
            }
            Edu::DeviceListUpdate(DeviceListUpdateContent { user_id, .. }) => {
                if user_id.server_name() == sender_servername {
                    services().users.mark_device_key_update(&user_id)?;
                }
            }
            Edu::DirectToDevice(DirectDeviceContent {
                sender,
                ev_type,
                message_id,
                messages,
            }) => {
                if sender.server_name() == sender_servername
                    // Check if this is a new transaction id
                    && services()
                        .transaction_ids
                        .existing_txnid(&sender, None, &message_id)?
                        .is_none()
                {
                    for (target_user_id, map) in &messages {
                        for (target_device_id_maybe, event) in map {
                            match target_device_id_maybe {
                                DeviceIdOrAllDevices::DeviceId(target_device_id) => {
                                    services().users.add_to_device_event(
                                        &sender,
                                        target_user_id,
                                        target_device_id,
                                        &ev_type.to_string(),
                                        event.deserialize_as().map_err(|e| {
                                            warn!("To-Device event is invalid: {event:?} {e}");
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
            }
            Edu::SigningKeyUpdate(SigningKeyUpdateContent {
                user_id,
                master_key,
                self_signing_key,
            }) => {
                if user_id.server_name() == sender_servername {
                    if let Some(master_key) = master_key {
                        services().users.add_cross_signing_keys(
                            &user_id,
                            &master_key,
                            &self_signing_key,
                            &None,
                            true,
                        )?;
                    }
                }
            }
            Edu::_Custom(_) => {}
        }
    }

    Ok(send_transaction_message::v1::Response { pdus: resolved_map })
}

/// # `GET /_matrix/federation/v1/event/{eventId}`
///
/// Retrieves a single event from the server.
///
/// - Only works if a user of this server is currently invited or joined the room
pub async fn get_event_route(
    body: Ruma<get_event::v1::Request>,
) -> Result<get_event::v1::Response> {
    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    let event = services()
        .rooms
        .timeline
        .get_pdu_json(&body.event_id)?
        .ok_or_else(|| {
            warn!("Event not found, event ID: {:?}", &body.event_id);
            Error::BadRequest(ErrorKind::NotFound, "Event not found.")
        })?;

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
            ErrorKind::forbidden(),
            "Server is not in room",
        ));
    }

    if !services().rooms.state_accessor.server_can_see_event(
        sender_servername,
        room_id,
        &body.event_id,
    )? {
        return Err(Error::BadRequest(
            ErrorKind::forbidden(),
            "Server is not allowed to see event.",
        ));
    }

    Ok(get_event::v1::Response {
        origin: services().globals.server_name().to_owned(),
        origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
        pdu: PduEvent::convert_to_outgoing_federation_event(event),
    })
}

/// # `GET /_matrix/federation/v1/backfill/<room_id>`
///
/// Retrieves events from before the sender joined the room, if the room's
/// history visibility allows.
pub async fn get_backfill_route(
    body: Ruma<get_backfill::v1::Request>,
) -> Result<get_backfill::v1::Response> {
    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    debug!("Got backfill request from: {}", sender_servername);

    if !services()
        .rooms
        .state_cache
        .server_in_room(sender_servername, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::forbidden(),
            "Server is not in room.",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(sender_servername, &body.room_id)?;

    let until = body
        .v
        .iter()
        .map(|eventid| services().rooms.timeline.get_pdu_count(eventid))
        .filter_map(|r| r.ok().flatten())
        .max()
        .ok_or(Error::BadRequest(
            ErrorKind::InvalidParam,
            "No known eventid in v",
        ))?;

    let limit = body.limit.min(uint!(100));

    let all_events = services()
        .rooms
        .timeline
        .pdus_until(user_id!("@doesntmatter:conduit.rs"), &body.room_id, until)?
        .take(limit.try_into().unwrap());

    let events = all_events
        .filter_map(|r| r.ok())
        .filter(|(_, e)| {
            matches!(
                services().rooms.state_accessor.server_can_see_event(
                    sender_servername,
                    &e.room_id,
                    &e.event_id,
                ),
                Ok(true),
            )
        })
        .map(|(_, pdu)| services().rooms.timeline.get_pdu_json(&pdu.event_id))
        .filter_map(|r| r.ok().flatten())
        .map(PduEvent::convert_to_outgoing_federation_event)
        .collect();

    Ok(get_backfill::v1::Response {
        origin: services().globals.server_name().to_owned(),
        origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
        pdus: events,
    })
}

/// # `POST /_matrix/federation/v1/get_missing_events/{roomId}`
///
/// Retrieves events that the sender is missing.
pub async fn get_missing_events_route(
    body: Ruma<get_missing_events::v1::Request>,
) -> Result<get_missing_events::v1::Response> {
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
            ErrorKind::forbidden(),
            "Server is not in room",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(sender_servername, &body.room_id)?;

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

            if !services().rooms.state_accessor.server_can_see_event(
                sender_servername,
                &body.room_id,
                &queued_events[i],
            )? {
                i += 1;
                continue;
            }

            queued_events.extend_from_slice(
                &serde_json::from_value::<Vec<OwnedEventId>>(
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
    body: Ruma<get_event_authorization::v1::Request>,
) -> Result<get_event_authorization::v1::Response> {
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
            ErrorKind::forbidden(),
            "Server is not in room.",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(sender_servername, &body.room_id)?;

    let event = services()
        .rooms
        .timeline
        .get_pdu_json(&body.event_id)?
        .ok_or_else(|| {
            warn!("Event not found, event ID: {:?}", &body.event_id);
            Error::BadRequest(ErrorKind::NotFound, "Event not found.")
        })?;

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
    body: Ruma<get_room_state::v1::Request>,
) -> Result<get_room_state::v1::Response> {
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
            ErrorKind::forbidden(),
            "Server is not in room.",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(sender_servername, &body.room_id)?;

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
        .into_values()
        .map(|id| {
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
            .filter_map(
                |id| match services().rooms.timeline.get_pdu_json(&id).ok()? {
                    Some(json) => Some(PduEvent::convert_to_outgoing_federation_event(json)),
                    None => {
                        error!("Could not find event json for {id} in db.");
                        None
                    }
                },
            )
            .collect(),
        pdus,
    })
}

/// # `GET /_matrix/federation/v1/state_ids/{roomId}`
///
/// Retrieves the current state of the room.
pub async fn get_room_state_ids_route(
    body: Ruma<get_room_state_ids::v1::Request>,
) -> Result<get_room_state_ids::v1::Response> {
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
            ErrorKind::forbidden(),
            "Server is not in room.",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(sender_servername, &body.room_id)?;

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
        .into_values()
        .map(|id| (*id).to_owned())
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
    body: Ruma<prepare_join_event::v1::Request>,
) -> Result<prepare_join_event::v1::Response> {
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
        .acl_check(sender_servername, &body.room_id)?;

    let mutex_state = Arc::clone(
        services()
            .globals
            .roomid_mutex_state
            .write()
            .await
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
            JoinRule::Restricted { .. } | JoinRule::KnockRestricted { .. }
        ) {
            return Err(Error::BadRequest(
                ErrorKind::UnableToAuthorizeJoin,
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

    let (_pdu, mut pdu_json) = services().rooms.timeline.create_hash_and_sign_event(
        PduBuilder {
            event_type: TimelineEventType::RoomMember,
            content,
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
            timestamp: None,
        },
        &body.user_id,
        &body.room_id,
        &state_lock,
    )?;

    drop(state_lock);

    pdu_json.remove("event_id");

    Ok(prepare_join_event::v1::Response {
        room_version: Some(room_version_id),
        event: to_raw_value(&pdu_json).expect("CanonicalJson can be serialized to JSON"),
    })
}

async fn create_join_event(
    sender_servername: &ServerName,
    room_id: &RoomId,
    pdu: &RawJsonValue,
) -> Result<create_join_event::v1::RoomState> {
    if !services().rooms.metadata.exists(room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Room is unknown to this server.",
        ));
    }

    services()
        .rooms
        .event_handler
        .acl_check(sender_servername, room_id)?;

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
            JoinRule::Restricted { .. } | JoinRule::KnockRestricted { .. }
        ) {
            return Err(Error::BadRequest(
                ErrorKind::UnableToAuthorizeJoin,
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
    let room_version_id = services().rooms.state.get_room_version(room_id)?;
    let (event_id, value) = match gen_event_id_canonical_json(pdu, &room_version_id) {
        Ok(t) => t,
        Err(_) => {
            // Event could not be converted to canonical json
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Could not convert event to canonical json.",
            ));
        }
    };

    let origin: OwnedServerName = serde_json::from_value(
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
            .await
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
        .get_auth_chain(room_id, state_ids.values().cloned().collect())
        .await?;

    let servers = services()
        .rooms
        .state_cache
        .room_servers(room_id)
        .filter_map(|r| r.ok())
        .filter(|server| &**server != services().globals.server_name());

    services().sending.send_pdu(servers, &pdu_id)?;

    Ok(create_join_event::v1::RoomState {
        auth_chain: auth_chain_ids
            .filter_map(|id| services().rooms.timeline.get_pdu_json(&id).ok().flatten())
            .map(PduEvent::convert_to_outgoing_federation_event)
            .collect(),
        state: state_ids
            .iter()
            .filter_map(|(_, id)| services().rooms.timeline.get_pdu_json(id).ok().flatten())
            .map(PduEvent::convert_to_outgoing_federation_event)
            .collect(),
        event: None, // TODO: handle restricted joins
    })
}

/// # `PUT /_matrix/federation/v1/send_join/{roomId}/{eventId}`
///
/// Submits a signed join event.
pub async fn create_join_event_v1_route(
    body: Ruma<create_join_event::v1::Request>,
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
    body: Ruma<create_join_event::v2::Request>,
) -> Result<create_join_event::v2::Response> {
    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    let create_join_event::v1::RoomState {
        auth_chain,
        state,
        event,
    } = create_join_event(sender_servername, &body.room_id, &body.pdu).await?;
    let room_state = create_join_event::v2::RoomState {
        members_omitted: false,
        auth_chain,
        state,
        event,
        servers_in_room: None,
    };

    Ok(create_join_event::v2::Response { room_state })
}

/// # `PUT /_matrix/federation/v2/invite/{roomId}/{eventId}`
///
/// Invites a remote user to a room.
pub async fn create_invite_route(
    body: Ruma<create_invite::v2::Request>,
) -> Result<create_invite::v2::Response> {
    let sender_servername = body
        .sender_servername
        .as_ref()
        .expect("server is authenticated");

    services()
        .rooms
        .event_handler
        .acl_check(sender_servername, &body.room_id)?;

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
            .expect("Event format validated when event was hashed")
    ))
    .expect("ruma's reference hashes are valid event ids");

    // Add event_id back
    signed_event.insert(
        "event_id".to_owned(),
        CanonicalJsonValue::String(event_id.to_string()),
    );

    let sender: OwnedUserId = serde_json::from_value(
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

    // If we are active in the room, the remote server will notify us about the join via /send
    if !services()
        .rooms
        .state_cache
        .server_in_room(services().globals.server_name(), &body.room_id)?
    {
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
    body: Ruma<get_devices::v1::Request>,
) -> Result<get_devices::v1::Response> {
    if body.user_id.server_name() != services().globals.server_name() {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Tried to access user from other server.",
        ));
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
        master_key: services().users.get_master_key(None, &body.user_id, &|u| {
            u.server_name() == sender_servername
        })?,
        self_signing_key: services()
            .users
            .get_self_signing_key(None, &body.user_id, &|u| {
                u.server_name() == sender_servername
            })?,
    })
}

/// # `GET /_matrix/federation/v1/query/directory`
///
/// Resolve a room alias to a room id.
pub async fn get_room_information_route(
    body: Ruma<get_room_information::v1::Request>,
) -> Result<get_room_information::v1::Response> {
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
    body: Ruma<get_profile_information::v1::Request>,
) -> Result<get_profile_information::v1::Response> {
    if body.user_id.server_name() != services().globals.server_name() {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Tried to access user from other server.",
        ));
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
    if body
        .device_keys
        .iter()
        .any(|(u, _)| u.server_name() != services().globals.server_name())
    {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Tried to access user from other server.",
        ));
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
    if body
        .one_time_keys
        .iter()
        .any(|(u, _)| u.server_name() != services().globals.server_name())
    {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Tried to access user from other server.",
        ));
    }

    let result = claim_keys_helper(&body.one_time_keys).await?;

    Ok(claim_keys::v1::Response {
        one_time_keys: result.one_time_keys,
    })
}

/// # `GET /_matrix/federation/v1/openid/userinfo`
///
/// Get information about the user that generated the OpenID token.
pub async fn get_openid_userinfo_route(
    body: Ruma<get_openid_userinfo::v1::Request>,
) -> Result<get_openid_userinfo::v1::Response> {
    Ok(get_openid_userinfo::v1::Response::new(
        services()
            .users
            .find_from_openid_token(&body.access_token)?
            .ok_or_else(|| {
                Error::BadRequest(
                    ErrorKind::Unauthorized,
                    "OpenID token has expired or does not exist.",
                )
            })?,
    ))
}

/// # `GET /.well-known/matrix/server`
///
/// Returns the federation server discovery information.
pub async fn well_known_server(
    _body: Ruma<discover_homeserver::Request>,
) -> Result<discover_homeserver::Response> {
    Ok(discover_homeserver::Response {
        server: services().globals.well_known_server(),
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
