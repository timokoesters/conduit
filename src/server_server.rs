use crate::{client_server, ConduitResult, Database, Error, Result, Ruma};
use http::header::{HeaderValue, AUTHORIZATION};
use rocket::{get, post, put, response::content::Json, State};
use ruma::api::{
    client,
    federation::{
        directory::get_public_rooms,
        discovery::{
            get_server_keys, get_server_version::v1 as get_server_version, ServerKey, VerifyKey,
        },
        transactions::send_transaction_message,
    },
    OutgoingRequest,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    convert::TryFrom,
    fmt::Debug,
    time::{Duration, SystemTime},
};

pub async fn request_well_known(db: &crate::Database, destination: &str) -> Option<String> {
    let body: serde_json::Value = serde_json::from_str(
        &db.globals
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
    db: &crate::Database,
    destination: String,
    request: T,
) -> Result<T::IncomingResponse>
where
    T: Debug,
{
    let actual_destination = "https://".to_owned()
        + &request_well_known(db, &destination)
            .await
            .unwrap_or(destination.clone() + ":8448");

    let mut http_request = request
        .try_into_http_request(&actual_destination, Some(""))
        .unwrap();

    let mut request_map = serde_json::Map::new();

    if !http_request.body().is_empty() {
        request_map.insert(
            "content".to_owned(),
            serde_json::from_slice(http_request.body()).unwrap(),
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
        db.globals.server_name().as_str().into(),
    );
    request_map.insert("destination".to_owned(), destination.into());

    let mut request_json = request_map.into();
    ruma::signatures::sign_json(
        db.globals.server_name().as_str(),
        db.globals.keypair(),
        &mut request_json,
    )
    .unwrap();

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
                    db.globals.server_name(),
                    s.0,
                    s.1
                ))
                .unwrap(),
            );
        }
    }

    let reqwest_request = reqwest::Request::try_from(http_request)
        .expect("all http requests are valid reqwest requests");

    let reqwest_response = db.globals.reqwest_client().execute(reqwest_request).await;

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
            Ok(
                T::IncomingResponse::try_from(http_response.body(body).unwrap())
                    .expect("TODO: error handle other server errors"),
            )
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg_attr(feature = "conduit_bin", get("/.well-known/matrix/server"))]
pub fn well_known_server() -> Json<String> {
    rocket::response::content::Json(json!({ "m.server": "pc.koesters.xyz:59003"}).to_string())
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
pub async fn get_public_rooms_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms::v1::IncomingRequest>,
) -> ConduitResult<get_public_rooms::v1::Response> {
    let Ruma {
        body:
            get_public_rooms::v1::IncomingRequest {
                room_network: _room_network, // TODO
                limit,
                since,
            },
        sender_id,
        device_id,
        json_body,
    } = body;

    let client::r0::directory::get_public_rooms_filtered::Response {
        chunk,
        prev_batch,
        next_batch,
        total_room_count_estimate,
    } = client_server::get_public_rooms_filtered_route(
        db,
        Ruma {
            body: client::r0::directory::get_public_rooms_filtered::IncomingRequest {
                filter: None,
                limit,
                room_network: ruma::directory::RoomNetwork::Matrix,
                server: None,
                since,
            },
            sender_id,
            device_id,
            json_body,
        },
    )
    .await?
    .0;

    Ok(get_public_rooms::v1::Response {
        chunk: chunk
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
        prev_batch,
        next_batch,
        total_room_count_estimate,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/federation/v1/send/<_>", data = "<body>")
)]
pub fn send_transaction_message_route<'a>(
    _db: State<'a, Database>,
    body: Ruma<send_transaction_message::v1::IncomingRequest>,
) -> ConduitResult<send_transaction_message::v1::Response> {
    dbg!(&*body);
    Ok(send_transaction_message::v1::Response {
        pdus: BTreeMap::new(),
    }
    .into())
}
