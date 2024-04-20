use std::{collections::BTreeMap, iter::FromIterator, str};

use axum::{
    async_trait,
    body::{Full, HttpBody},
    extract::{rejection::TypedHeaderRejectionReason, FromRequest, Path, TypedHeader},
    headers::{
        authorization::{Bearer, Credentials},
        Authorization,
    },
    response::{IntoResponse, Response},
    BoxError, RequestExt, RequestPartsExt,
};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use http::{Request, StatusCode};
use ruma::{
    api::{client::error::ErrorKind, AuthScheme, IncomingRequest, OutgoingResponse},
    CanonicalJsonValue, OwnedDeviceId, OwnedServerName, OwnedUserId, UserId,
};
use serde::Deserialize;
use tracing::{debug, error, warn};

use super::{Ruma, RumaResponse};
use crate::{service::appservice::RegistrationInfo, services, Error, Result};

enum Token {
    Appservice(Box<RegistrationInfo>),
    User((OwnedUserId, OwnedDeviceId)),
    Invalid,
    None,
}

#[async_trait]
impl<T, S, B> FromRequest<S, B> for Ruma<T>
where
    T: IncomingRequest,
    B: HttpBody + Send + 'static,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    type Rejection = Error;

    async fn from_request(req: Request<B>, _state: &S) -> Result<Self, Self::Rejection> {
        #[derive(Deserialize)]
        struct QueryParams {
            access_token: Option<String>,
            user_id: Option<String>,
        }

        let (mut parts, mut body) = match req.with_limited_body() {
            Ok(limited_req) => {
                let (parts, body) = limited_req.into_parts();
                let body = to_bytes(body)
                    .await
                    .map_err(|_| Error::BadRequest(ErrorKind::MissingToken, "Missing token."))?;
                (parts, body)
            }
            Err(original_req) => {
                let (parts, body) = original_req.into_parts();
                let body = to_bytes(body)
                    .await
                    .map_err(|_| Error::BadRequest(ErrorKind::MissingToken, "Missing token."))?;
                (parts, body)
            }
        };

        let metadata = T::METADATA;
        let auth_header: Option<TypedHeader<Authorization<Bearer>>> = parts.extract().await?;
        let path_params: Path<Vec<String>> = parts.extract().await?;

        let query = parts.uri.query().unwrap_or_default();
        let query_params: QueryParams = match serde_html_form::from_str(query) {
            Ok(params) => params,
            Err(e) => {
                error!(%query, "Failed to deserialize query parameters: {}", e);
                return Err(Error::BadRequest(
                    ErrorKind::Unknown,
                    "Failed to read query parameters",
                ));
            }
        };

        let token = match &auth_header {
            Some(TypedHeader(Authorization(bearer))) => Some(bearer.token()),
            None => query_params.access_token.as_deref(),
        };

        let token = if let Some(token) = token {
            if let Some(reg_info) = services().appservice.find_from_token(token).await {
                Token::Appservice(Box::new(reg_info.clone()))
            } else if let Some((user_id, device_id)) = services().users.find_from_token(token)? {
                Token::User((user_id, OwnedDeviceId::from(device_id)))
            } else {
                Token::Invalid
            }
        } else {
            Token::None
        };

        let mut json_body = serde_json::from_slice::<CanonicalJsonValue>(&body).ok();

        let (sender_user, sender_device, sender_servername, from_appservice) =
            match (metadata.authentication, token) {
                (_, Token::Invalid) => {
                    return Err(Error::BadRequest(
                        ErrorKind::UnknownToken { soft_logout: false },
                        "Unknown access token.",
                    ))
                }
                (
                    AuthScheme::AccessToken | AuthScheme::AccessTokenOptional,
                    Token::Appservice(info),
                ) => {
                    let user_id = query_params
                        .user_id
                        .map_or_else(
                            || {
                                UserId::parse_with_server_name(
                                    info.registration.sender_localpart.as_str(),
                                    services().globals.server_name(),
                                )
                            },
                            UserId::parse,
                        )
                        .map_err(|_| {
                            Error::BadRequest(ErrorKind::InvalidUsername, "Username is invalid.")
                        })?;
                    if !services().users.exists(&user_id)? {
                        return Err(Error::BadRequest(
                            ErrorKind::Forbidden,
                            "User does not exist.",
                        ));
                    }

                    // TODO: Check if appservice is allowed to be that user
                    (Some(user_id), None, None, true)
                }
                (AuthScheme::None | AuthScheme::AppserviceToken, Token::Appservice(_)) => {
                    (None, None, None, true)
                }
                (AuthScheme::AccessToken, Token::None) => {
                    return Err(Error::BadRequest(
                        ErrorKind::MissingToken,
                        "Missing access token.",
                    ));
                }
                (
                    AuthScheme::AccessToken | AuthScheme::AccessTokenOptional | AuthScheme::None,
                    Token::User((user_id, device_id)),
                ) => (Some(user_id), Some(device_id), None, false),
                (AuthScheme::ServerSignatures, Token::None) => {
                    let TypedHeader(Authorization(x_matrix)) = parts
                        .extract::<TypedHeader<Authorization<XMatrix>>>()
                        .await
                        .map_err(|e| {
                            warn!("Missing or invalid Authorization header: {}", e);

                            let msg = match e.reason() {
                                TypedHeaderRejectionReason::Missing => {
                                    "Missing Authorization header."
                                }
                                TypedHeaderRejectionReason::Error(_) => {
                                    "Invalid X-Matrix signatures."
                                }
                                _ => "Unknown header-related error",
                            };

                            Error::BadRequest(ErrorKind::Forbidden, msg)
                        })?;

                    let origin_signatures = BTreeMap::from_iter([(
                        x_matrix.key.clone(),
                        CanonicalJsonValue::String(x_matrix.sig),
                    )]);

                    let signatures = BTreeMap::from_iter([(
                        x_matrix.origin.as_str().to_owned(),
                        CanonicalJsonValue::Object(origin_signatures),
                    )]);

                    let mut request_map = BTreeMap::from_iter([
                        (
                            "method".to_owned(),
                            CanonicalJsonValue::String(parts.method.to_string()),
                        ),
                        (
                            "uri".to_owned(),
                            CanonicalJsonValue::String(parts.uri.to_string()),
                        ),
                        (
                            "origin".to_owned(),
                            CanonicalJsonValue::String(x_matrix.origin.as_str().to_owned()),
                        ),
                        (
                            "destination".to_owned(),
                            CanonicalJsonValue::String(
                                services().globals.server_name().as_str().to_owned(),
                            ),
                        ),
                        (
                            "signatures".to_owned(),
                            CanonicalJsonValue::Object(signatures),
                        ),
                    ]);

                    if let Some(json_body) = &json_body {
                        request_map.insert("content".to_owned(), json_body.clone());
                    };

                    let keys_result = services()
                        .rooms
                        .event_handler
                        .fetch_signing_keys(&x_matrix.origin, vec![x_matrix.key.to_owned()])
                        .await;

                    let keys = match keys_result {
                        Ok(b) => b,
                        Err(e) => {
                            warn!("Failed to fetch signing keys: {}", e);
                            return Err(Error::BadRequest(
                                ErrorKind::Forbidden,
                                "Failed to fetch signing keys.",
                            ));
                        }
                    };

                    let pub_key_map =
                        BTreeMap::from_iter([(x_matrix.origin.as_str().to_owned(), keys)]);

                    match ruma::signatures::verify_json(&pub_key_map, &request_map) {
                        Ok(()) => (None, None, Some(x_matrix.origin), false),
                        Err(e) => {
                            warn!(
                                "Failed to verify json request from {}: {}\n{:?}",
                                x_matrix.origin, e, request_map
                            );

                            if parts.uri.to_string().contains('@') {
                                warn!(
                                    "Request uri contained '@' character. Make sure your \
                                         reverse proxy gives Conduit the raw uri (apache: use \
                                         nocanon)"
                                );
                            }

                            return Err(Error::BadRequest(
                                ErrorKind::Forbidden,
                                "Failed to verify X-Matrix signatures.",
                            ));
                        }
                    }
                }
                (
                    AuthScheme::None
                    | AuthScheme::AppserviceToken
                    | AuthScheme::AccessTokenOptional,
                    Token::None,
                ) => (None, None, None, false),
                (AuthScheme::ServerSignatures, Token::Appservice(_) | Token::User(_)) => {
                    return Err(Error::BadRequest(
                        ErrorKind::Unauthorized,
                        "Only server signatures should be used on this endpoint.",
                    ));
                }
                (AuthScheme::AppserviceToken, Token::User(_)) => {
                    return Err(Error::BadRequest(
                        ErrorKind::Unauthorized,
                        "Only appservice access tokens should be used on this endpoint.",
                    ));
                }
            };

        let mut http_request = http::Request::builder().uri(parts.uri).method(parts.method);
        *http_request.headers_mut().unwrap() = parts.headers;

        if let Some(CanonicalJsonValue::Object(json_body)) = &mut json_body {
            let user_id = sender_user.clone().unwrap_or_else(|| {
                UserId::parse_with_server_name("", services().globals.server_name())
                    .expect("we know this is valid")
            });

            let uiaa_request = json_body
                .get("auth")
                .and_then(|auth| auth.as_object())
                .and_then(|auth| auth.get("session"))
                .and_then(|session| session.as_str())
                .and_then(|session| {
                    services().uiaa.get_uiaa_request(
                        &user_id,
                        &sender_device.clone().unwrap_or_else(|| "".into()),
                        session,
                    )
                });

            if let Some(CanonicalJsonValue::Object(initial_request)) = uiaa_request {
                for (key, value) in initial_request {
                    json_body.entry(key).or_insert(value);
                }
            }

            let mut buf = BytesMut::new().writer();
            serde_json::to_writer(&mut buf, json_body).expect("value serialization can't fail");
            body = buf.into_inner().freeze();
        }

        let http_request = http_request.body(&*body).unwrap();

        debug!("{:?}", http_request);

        let body = T::try_from_http_request(http_request, &path_params).map_err(|e| {
            warn!("try_from_http_request failed: {:?}", e);
            debug!("JSON body: {:?}", json_body);
            Error::BadRequest(ErrorKind::BadJson, "Failed to deserialize request.")
        })?;

        Ok(Ruma {
            body,
            sender_user,
            sender_device,
            sender_servername,
            from_appservice,
            json_body,
        })
    }
}

struct XMatrix {
    origin: OwnedServerName,
    key: String, // KeyName?
    sig: String,
}

impl Credentials for XMatrix {
    const SCHEME: &'static str = "X-Matrix";

    fn decode(value: &http::HeaderValue) -> Option<Self> {
        debug_assert!(
            value.as_bytes().starts_with(b"X-Matrix "),
            "HeaderValue to decode should start with \"X-Matrix ..\", received = {value:?}",
        );

        let parameters = str::from_utf8(&value.as_bytes()["X-Matrix ".len()..])
            .ok()?
            .trim_start();

        let mut origin = None;
        let mut key = None;
        let mut sig = None;

        for entry in parameters.split_terminator(',') {
            let (name, value) = entry.split_once('=')?;

            // It's not at all clear why some fields are quoted and others not in the spec,
            // let's simply accept either form for every field.
            let value = value
                .strip_prefix('"')
                .and_then(|rest| rest.strip_suffix('"'))
                .unwrap_or(value);

            // FIXME: Catch multiple fields of the same name
            match name {
                "origin" => origin = Some(value.try_into().ok()?),
                "key" => key = Some(value.to_owned()),
                "sig" => sig = Some(value.to_owned()),
                _ => debug!(
                    "Unexpected field `{}` in X-Matrix Authorization header",
                    name
                ),
            }
        }

        Some(Self {
            origin: origin?,
            key: key?,
            sig: sig?,
        })
    }

    fn encode(&self) -> http::HeaderValue {
        todo!()
    }
}

impl<T: OutgoingResponse> IntoResponse for RumaResponse<T> {
    fn into_response(self) -> Response {
        match self.0.try_into_http_response::<BytesMut>() {
            Ok(res) => res.map(BytesMut::freeze).map(Full::new).into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}

// copied from hyper under the following license:
// Copyright (c) 2014-2021 Sean McArthur

// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:

// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.

// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN
// THE SOFTWARE.
pub(crate) async fn to_bytes<T>(body: T) -> Result<Bytes, T::Error>
where
    T: HttpBody,
{
    futures_util::pin_mut!(body);

    // If there's only 1 chunk, we can just return Buf::to_bytes()
    let mut first = if let Some(buf) = body.data().await {
        buf?
    } else {
        return Ok(Bytes::new());
    };

    let second = if let Some(buf) = body.data().await {
        buf?
    } else {
        return Ok(first.copy_to_bytes(first.remaining()));
    };

    // With more than 1 buf, we gotta flatten into a Vec first.
    let cap = first.remaining() + second.remaining() + body.size_hint().lower() as usize;
    let mut vec = Vec::with_capacity(cap);
    vec.put(first);
    vec.put(second);

    while let Some(buf) = body.data().await {
        vec.put(buf?);
    }

    Ok(vec.into())
}
