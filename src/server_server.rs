use crate::{client_server, ConduitResult, Database, Error, PduEvent, Result, Ruma};
use http::header::{HeaderValue, AUTHORIZATION};
use log::warn;
use rocket::{get, post, put, response::content::Json, State};
use ruma::{
    api::{
        federation::{
            directory::{get_public_rooms, get_public_rooms_filtered},
            discovery::{
                get_server_keys, get_server_version::v1 as get_server_version, ServerKey, VerifyKey,
            },
            transactions::send_transaction_message,
        },
        OutgoingRequest,
    },
    directory::{IncomingFilter, IncomingRoomNetwork},
    EventId, ServerName,
};
use std::{
    collections::BTreeMap,
    convert::TryFrom,
    fmt::Debug,
    time::{Duration, SystemTime},
};

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

pub async fn send_request<T: OutgoingRequest>(
    globals: &crate::database::globals::Globals,
    destination: Box<ServerName>,
    request: T,
) -> Result<T::IncomingResponse>
where
    T: Debug,
{
    let actual_destination = "https://".to_owned()
        + &request_well_known(globals, &destination.as_str())
            .await
            .unwrap_or_else(|| {
                let mut destination = destination.as_str().to_owned();
                if destination.find(':').is_none() {
                    destination += ":8448";
                }
                destination
            });

    let mut http_request = request
        .try_into_http_request(&actual_destination, Some(""))
        .map_err(|e| {
            warn!("{}: {}", actual_destination, e);
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

    let mut request_json = request_map.into();
    ruma::signatures::sign_json(
        globals.server_name().as_str(),
        globals.keypair(),
        &mut request_json,
    )
    .expect("our request json is what ruma expects");

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

    let reqwest_response = globals.reqwest_client().execute(reqwest_request).await;

    // Because reqwest::Response -> http::Response is complicated:
    match reqwest_response {
        Ok(mut reqwest_response) => {
            let status = reqwest_response.status();
            let mut http_response = http::Response::builder().status(status);
            let headers = http_response.headers_mut().unwrap();

            for (k, v) in reqwest_response.headers_mut().drain() {
                if let Some(key) = k {
                    headers.insert(key, v);
                }
            }

            let body = reqwest_response
                .bytes()
                .await
                .unwrap()
                .into_iter()
                .collect();

            let response = T::IncomingResponse::try_from(
                http_response
                    .body(body)
                    .expect("reqwest body is valid http body"),
            );
            response.map_err(|e| {
                warn!("Server returned bad response: {:?}", e);
                Error::BadServerResponse("Server returned bad response.")
            })
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/federation/v1/version"))]
pub fn get_server_version() -> ConduitResult<get_server_version::Response> {
    Ok(get_server_version::Response {
        server: Some(get_server_version::Server {
            name: Some("Conduit".to_owned()),
            version: Some(env!("CARGO_PKG_VERSION").to_owned()),
        }),
    }
    .into())
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/key/v2/server"))]
pub fn get_server_keys(db: State<'_, Database>) -> Json<String> {
    let mut verify_keys = BTreeMap::new();
    verify_keys.insert(
        format!("ed25519:{}", db.globals.keypair().version()),
        VerifyKey {
            key: base64::encode_config(db.globals.keypair().public_key(), base64::STANDARD_NO_PAD),
        },
    );
    let mut response = serde_json::from_slice(
        http::Response::try_from(get_server_keys::v2::Response {
            server_key: ServerKey {
                server_name: db.globals.server_name().to_owned(),
                verify_keys,
                old_verify_keys: BTreeMap::new(),
                signatures: BTreeMap::new(),
                valid_until_ts: SystemTime::now() + Duration::from_secs(60 * 2),
            },
        })
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
    Json(response.to_string())
}

#[cfg_attr(feature = "conduit_bin", get("/_matrix/key/v2/server/<_>"))]
pub fn get_server_keys_deprecated(db: State<'_, Database>) -> Json<String> {
    get_server_keys(db)
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/federation/v1/publicRooms", data = "<body>")
)]
pub async fn get_public_rooms_filtered_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms_filtered::v1::Request<'_>>,
) -> ConduitResult<get_public_rooms_filtered::v1::Response> {
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
                Ok::<_, Error>(
                    serde_json::from_str(
                        &serde_json::to_string(&c)
                            .expect("PublicRoomsChunk::to_string always works"),
                    )
                    .expect("federation and client-server PublicRoomsChunk are the same type"),
                )
            })
            .filter_map(|r| r.ok())
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
pub async fn get_public_rooms_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms::v1::Request<'_>>,
) -> ConduitResult<get_public_rooms::v1::Response> {
    let response = client_server::get_public_rooms_filtered_helper(
        &db,
        None,
        body.limit,
        body.since.as_deref(),
        &IncomingFilter {
            generic_search_term: None,
        },
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
                Ok::<_, Error>(
                    serde_json::from_str(
                        &serde_json::to_string(&c)
                            .expect("PublicRoomsChunk::to_string always works"),
                    )
                    .expect("federation and client-server PublicRoomsChunk are the same type"),
                )
            })
            .filter_map(|r| r.ok())
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
pub async fn send_transaction_message_route<'a>(
    db: State<'a, Database>,
    body: Ruma<send_transaction_message::v1::Request<'_>>,
) -> ConduitResult<send_transaction_message::v1::Response> {
    dbg!(&*body);

    let mut resolved_map = BTreeMap::new();
    for pdu in &body.pdus {
        let (event_id, value) = process_incoming_pdu(pdu);
        let event = serde_json::from_value::<state_res::StateEvent>(value.clone()).unwrap();
        let room_id = event.room_id().expect("found PduStub event");

        // if event.state_key().is_none() {
        //     resolved_map.insert(event_id, Ok::<(), String>(()));
        //     db.rooms.append_pdu(
        //         &PduEvent::from(&event),
        //         &value,
        //         &db.globals,
        //         &db.account_data,
        //     )?;

        //     continue;
        // }

        let our_current_state = db.rooms.room_state_full(room_id)?;

        let get_state_response = send_request(
            &db.globals,
            body.body.origin.clone(),
            ruma::api::federation::event::get_room_state::v1::Request {
                room_id,
                event_id: &event_id,
            },
        )
        .await
        .unwrap();
        let their_current_state = get_state_response
            .pdus
            .iter()
            .chain(get_state_response.auth_chain.iter()) // add auth events
            .map(|pdu| {
                let (event_id, json) = process_incoming_pdu(pdu);
                (
                    event_id,
                    std::sync::Arc::new(
                        serde_json::from_value::<state_res::StateEvent>(json)
                            .expect("valid pdu json"),
                    ),
                )
            })
            .collect::<BTreeMap<_, _>>();

        match state_res::StateResolution::resolve(
            event.room_id().unwrap(),
            &ruma::RoomVersionId::Version5,
            &[
                our_current_state
                    .iter()
                    .map(|((ev, sk), v)| ((ev.clone(), sk.to_owned()), v.event_id.clone()))
                    .collect::<BTreeMap<_, _>>(),
                // TODO we may not want the auth events chained in here for resolution?
                their_current_state
                    .iter()
                    .map(|(_id, v)| ((v.kind(), v.state_key()), v.event_id().clone()))
                    .collect::<BTreeMap<_, _>>(),
            ],
            Some(
                our_current_state
                    .iter()
                    .map(|(_k, v)| (v.event_id.clone(), v.convert_for_state_res()))
                    .chain(
                        their_current_state
                            .iter()
                            .map(|(id, ev)| (id.clone(), ev.clone())),
                    )
                    .collect::<BTreeMap<_, _>>(),
            ),
            &db.rooms,
        ) {
            Ok(resolved) if resolved.values().any(|id| &event_id == id) => {
                let pdu = serde_json::from_value::<PduEvent>(value.clone())
                    .expect("all ruma pdus are conduit pdus");
                if db.rooms.exists(&pdu.room_id)? {
                    let pdu_id =
                        db.rooms
                            .append_pdu(&pdu, &value, &db.globals, &db.account_data)?;
                    db.rooms.append_to_state(&pdu_id, &pdu)?;
                    resolved_map.insert(event_id, Ok::<(), String>(()));
                }
            }
            // If the eventId is not found in the resolved state auth has failed
            Ok(_) => {
                // TODO have state_res give the actual auth error in this case
                resolved_map.insert(event_id, Err("This event failed authentication".into()));
            }
            Err(e) => {
                resolved_map.insert(event_id, Err(e.to_string()));
            }
        }
    }

    Ok(send_transaction_message::v1::Response { pdus: resolved_map }.into())
}

/// Generates a correct eventId for the incoming pdu.
///
/// Returns a `state_res::StateEvent` which can be converted freely and has accessor methods.
fn process_incoming_pdu(pdu: &ruma::Raw<ruma::events::pdu::Pdu>) -> (EventId, serde_json::Value) {
    let mut value = serde_json::to_value(pdu.json().get()).expect("all ruma pdus are json values");
    let event_id = EventId::try_from(&*format!(
        "${}",
        ruma::signatures::reference_hash(&value).expect("ruma can calculate reference hashes")
    ))
    .expect("ruma's reference hashes are valid event ids");

    value
        .as_object_mut()
        .expect("ruma pdus are json objects")
        .insert("event_id".to_owned(), event_id.to_string().into());

    (event_id, value)
}
