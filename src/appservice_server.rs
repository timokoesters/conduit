use crate::{utils, Error, Result};
use http::header::{HeaderValue, CONTENT_TYPE};
use log::warn;
use ruma::api::OutgoingRequest;
use std::{
    convert::{TryFrom, TryInto},
    fmt::Debug,
    time::Duration,
};

pub async fn send_request<T: OutgoingRequest>(
    globals: &crate::database::globals::Globals,
    registration: serde_yaml::Value,
    request: T,
) -> Result<T::IncomingResponse>
where
    T: Debug,
{
    let destination = registration.get("url").unwrap().as_str().unwrap();
    let hs_token = registration.get("hs_token").unwrap().as_str().unwrap();

    let mut http_request = request
        .try_into_http_request(&destination, Some(""))
        .unwrap();

    let mut parts = http_request.uri().clone().into_parts();
    let old_path_and_query = parts.path_and_query.unwrap().as_str().to_owned();
    let symbol = if old_path_and_query.contains('?') {
        "&"
    } else {
        "?"
    };

    parts.path_and_query = Some(
        (old_path_and_query + symbol + "access_token=" + hs_token)
            .parse()
            .unwrap(),
    );
    *http_request.uri_mut() = parts.try_into().expect("our manipulation is always valid");

    http_request.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_str("application/json").unwrap(),
    );

    let mut reqwest_request = reqwest::Request::try_from(http_request)
        .expect("all http requests are valid reqwest requests");

    *reqwest_request.timeout_mut() = Some(Duration::from_secs(30));

    let url = reqwest_request.url().clone();
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

            let status = reqwest_response.status();

            let body = reqwest_response
                .bytes()
                .await
                .unwrap_or_else(|e| {
                    warn!("server error: {}", e);
                    Vec::new().into()
                }) // TODO: handle timeout
                .into_iter()
                .collect::<Vec<_>>();

            if status != 200 {
                warn!(
                    "Appservice returned bad response {} {}\n{}\n{:?}",
                    destination,
                    status,
                    url,
                    utils::string_from_bytes(&body)
                );
            }

            let response = T::IncomingResponse::try_from(
                http_response
                    .body(body)
                    .expect("reqwest body is valid http body"),
            );
            response.map_err(|_| {
                warn!(
                    "Appservice returned invalid response bytes {}\n{}",
                    destination, url
                );
                Error::BadServerResponse("Server returned bad response.")
            })
        }
        Err(e) => Err(e.into()),
    }
}
