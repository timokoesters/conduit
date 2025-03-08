use std::{collections::BTreeMap, iter::FromIterator, str};

use axum::{
    async_trait,
    body::Body,
    extract::{FromRequest, Path},
    response::{IntoResponse, Response},
    RequestExt, RequestPartsExt,
};
use axum_extra::{
    headers::{authorization::Bearer, Authorization},
    typed_header::TypedHeaderRejectionReason,
    TypedHeader,
};
use bytes::{BufMut, BytesMut};
use http::{Request, StatusCode};
use ruma::{
    api::{
        client::error::ErrorKind, federation::authentication::XMatrix, AuthScheme, IncomingRequest,
        OutgoingResponse,
    },
    CanonicalJsonValue, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedUserId, UserId,
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
impl<T, S> FromRequest<S> for Ruma<T>
where
    T: IncomingRequest,
{
    type Rejection = Error;

    async fn from_request(req: Request<Body>, _state: &S) -> Result<Self, Self::Rejection> {
        #[derive(Deserialize)]
        struct QueryParams {
            access_token: Option<String>,
            user_id: Option<String>,
        }

        let (mut parts, mut body) = {
            let limited_req = req.with_limited_body();
            let (parts, body) = limited_req.into_parts();
            let body = axum::body::to_bytes(
                body,
                services()
                    .globals
                    .max_request_size()
                    .try_into()
                    .unwrap_or(usize::MAX),
            )
            .await
            .map_err(|_| Error::BadRequest(ErrorKind::MissingToken, "Missing token."))?;
            (parts, body)
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

        let (sender_user, sender_device, sender_servername, appservice_info) =
            match (metadata.authentication, token) {
                (_, Token::Invalid) => {
                    // OpenID endpoint uses a query param with the same name, drop this once query params for user auth are removed from the spec
                    if query_params.access_token.is_some() {
                        (None, None, None, None)
                    } else {
                        return Err(Error::BadRequest(
                            ErrorKind::UnknownToken { soft_logout: false },
                            "Unknown access token.",
                        ));
                    }
                }
                (
                    AuthScheme::AccessToken | AuthScheme::AppserviceToken,
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

                    if !info.is_user_match(&user_id) {
                        return Err(Error::BadRequest(
                            ErrorKind::Exclusive,
                            "User is not in namespace.",
                        ));
                    }

                    if !services().users.exists(&user_id)? {
                        return Err(Error::BadRequest(
                            ErrorKind::forbidden(),
                            "User does not exist.",
                        ));
                    }

                    (Some(user_id), None, None, Some(*info))
                }
                (
                    AuthScheme::None
                    | AuthScheme::AppserviceTokenOptional
                    | AuthScheme::AccessTokenOptional,
                    Token::Appservice(info),
                ) => (None, None, None, Some(*info)),
                (AuthScheme::AppserviceToken | AuthScheme::AccessToken, Token::None) => {
                    return Err(Error::BadRequest(
                        ErrorKind::MissingToken,
                        "Missing access token.",
                    ));
                }
                (
                    AuthScheme::AccessToken | AuthScheme::AccessTokenOptional | AuthScheme::None,
                    Token::User((user_id, device_id)),
                ) => (Some(user_id), Some(device_id), None, None),
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

                            Error::BadRequest(ErrorKind::forbidden(), msg)
                        })?;

                    if let Some(dest) = x_matrix.destination {
                        if dest != services().globals.server_name() {
                            return Err(Error::BadRequest(
                                ErrorKind::Unauthorized,
                                "X-Matrix destination field does not match server name.",
                            ));
                        }
                    };

                    let origin_signatures = BTreeMap::from_iter([(
                        x_matrix.key.clone(),
                        CanonicalJsonValue::String(x_matrix.sig.to_string()),
                    )]);

                    let signatures = BTreeMap::from_iter([(
                        x_matrix.origin.as_str().to_owned(),
                        CanonicalJsonValue::Object(
                            origin_signatures
                                .into_iter()
                                .map(|(k, v)| (k.to_string(), v))
                                .collect(),
                        ),
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
                        .fetch_signing_keys(&x_matrix.origin, vec![x_matrix.key.to_string()], false)
                        .await;

                    let keys = match keys_result {
                        Ok(b) => b,
                        Err(e) => {
                            warn!("Failed to fetch signing keys: {}", e);
                            return Err(Error::BadRequest(
                                ErrorKind::forbidden(),
                                "Failed to fetch signing keys.",
                            ));
                        }
                    };

                    // Only verify_keys that are currently valid should be used for validating requests
                    // as per MSC4029
                    let pub_key_map = BTreeMap::from_iter([(
                        x_matrix.origin.as_str().to_owned(),
                        if keys.valid_until_ts > MilliSecondsSinceUnixEpoch::now() {
                            keys.verify_keys
                                .into_iter()
                                .map(|(id, key)| (id, key.key))
                                .collect()
                        } else {
                            BTreeMap::new()
                        },
                    )]);

                    match ruma::signatures::verify_json(&pub_key_map, &request_map) {
                        Ok(()) => (None, None, Some(x_matrix.origin), None),
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
                                ErrorKind::forbidden(),
                                "Failed to verify X-Matrix signatures.",
                            ));
                        }
                    }
                }
                (
                    AuthScheme::None
                    | AuthScheme::AppserviceTokenOptional
                    | AuthScheme::AccessTokenOptional,
                    Token::None,
                ) => (None, None, None, None),
                (AuthScheme::ServerSignatures, Token::Appservice(_) | Token::User(_)) => {
                    return Err(Error::BadRequest(
                        ErrorKind::Unauthorized,
                        "Only server signatures should be used on this endpoint.",
                    ));
                }
                (
                    AuthScheme::AppserviceToken | AuthScheme::AppserviceTokenOptional,
                    Token::User(_),
                ) => {
                    return Err(Error::BadRequest(
                        ErrorKind::Unauthorized,
                        "Only appservice access tokens should be used on this endpoint.",
                    ));
                }
            };

        let mut http_request = Request::builder().uri(parts.uri).method(parts.method);
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
            appservice_info,
            json_body,
        })
    }
}

impl<T: OutgoingResponse> IntoResponse for RumaResponse<T> {
    fn into_response(self) -> Response {
        match self.0.try_into_http_response::<BytesMut>() {
            Ok(res) => res.map(BytesMut::freeze).map(Body::from).into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}
