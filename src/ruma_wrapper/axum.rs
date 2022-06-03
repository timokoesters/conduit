use std::{collections::BTreeMap, iter::FromIterator, str};

use axum::{
    async_trait,
    body::{Full, HttpBody},
    extract::{
        rejection::TypedHeaderRejectionReason, FromRequest, Path, RequestParts, TypedHeader,
    },
    headers::{
        authorization::{Bearer, Credentials},
        Authorization,
    },
    response::{IntoResponse, Response},
    BoxError,
};
use bytes::{BufMut, Bytes, BytesMut};
use http::StatusCode;
use ruma::{
    api::{client::error::ErrorKind, AuthScheme, IncomingRequest, OutgoingResponse},
    signatures::CanonicalJsonValue,
    OwnedDeviceId, OwnedServerName, UserId,
};
use serde::Deserialize;
use tracing::{debug, error, warn};

use super::{Ruma, RumaResponse};
use crate::{database::DatabaseGuard, server_server, Error, Result};

#[async_trait]
impl<T, B> FromRequest<B> for Ruma<T>
where
    T: IncomingRequest,
    B: HttpBody + Send,
    B::Data: Send,
    B::Error: Into<BoxError>,
{
    type Rejection = Error;

    async fn from_request(req: &mut RequestParts<B>) -> Result<Self, Self::Rejection> {
        #[derive(Deserialize)]
        struct QueryParams {
            access_token: Option<String>,
            user_id: Option<String>,
        }

        let metadata = T::METADATA;
        let db = DatabaseGuard::from_request(req).await?;
        let auth_header = Option::<TypedHeader<Authorization<Bearer>>>::from_request(req).await?;
        let path_params = Path::<Vec<String>>::from_request(req).await?;

        let query = req.uri().query().unwrap_or_default();
        let query_params: QueryParams = match ruma::serde::urlencoded::from_str(query) {
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

        let mut body = Bytes::from_request(req)
            .await
            .map_err(|_| Error::BadRequest(ErrorKind::MissingToken, "Missing token."))?;

        let mut json_body = serde_json::from_slice::<CanonicalJsonValue>(&body).ok();

        let appservices = db.appservice.all().unwrap();
        let appservice_registration = appservices.iter().find(|(_id, registration)| {
            registration
                .get("as_token")
                .and_then(|as_token| as_token.as_str())
                .map_or(false, |as_token| token == Some(as_token))
        });

        let (sender_user, sender_device, sender_servername, from_appservice) =
            if let Some((_id, registration)) = appservice_registration {
                match metadata.authentication {
                    AuthScheme::AccessToken | AuthScheme::QueryOnlyAccessToken => {
                        let user_id = query_params.user_id.map_or_else(
                            || {
                                UserId::parse_with_server_name(
                                    registration
                                        .get("sender_localpart")
                                        .unwrap()
                                        .as_str()
                                        .unwrap(),
                                    db.globals.server_name(),
                                )
                                .unwrap()
                            },
                            |s| UserId::parse(s).unwrap(),
                        );

                        if !db.users.exists(&user_id).unwrap() {
                            return Err(Error::BadRequest(
                                ErrorKind::Forbidden,
                                "User does not exist.",
                            ));
                        }

                        // TODO: Check if appservice is allowed to be that user
                        (Some(user_id), None, None, true)
                    }
                    AuthScheme::ServerSignatures => (None, None, None, true),
                    AuthScheme::None => (None, None, None, true),
                }
            } else {
                match metadata.authentication {
                    AuthScheme::AccessToken | AuthScheme::QueryOnlyAccessToken => {
                        let token = match token {
                            Some(token) => token,
                            _ => {
                                return Err(Error::BadRequest(
                                    ErrorKind::MissingToken,
                                    "Missing access token.",
                                ))
                            }
                        };

                        match db.users.find_from_token(token).unwrap() {
                            None => {
                                return Err(Error::BadRequest(
                                    ErrorKind::UnknownToken { soft_logout: false },
                                    "Unknown access token.",
                                ))
                            }
                            Some((user_id, device_id)) => (
                                Some(user_id),
                                Some(OwnedDeviceId::from(device_id)),
                                None,
                                false,
                            ),
                        }
                    }
                    AuthScheme::ServerSignatures => {
                        let TypedHeader(Authorization(x_matrix)) =
                            TypedHeader::<Authorization<XMatrix>>::from_request(req)
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
                                CanonicalJsonValue::String(req.method().to_string()),
                            ),
                            (
                                "uri".to_owned(),
                                CanonicalJsonValue::String(req.uri().to_string()),
                            ),
                            (
                                "origin".to_owned(),
                                CanonicalJsonValue::String(x_matrix.origin.as_str().to_owned()),
                            ),
                            (
                                "destination".to_owned(),
                                CanonicalJsonValue::String(
                                    db.globals.server_name().as_str().to_owned(),
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

                        let keys_result = server_server::fetch_signing_keys(
                            &db,
                            &x_matrix.origin,
                            vec![x_matrix.key.to_owned()],
                        )
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

                                if req.uri().to_string().contains('@') {
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
                    AuthScheme::None => (None, None, None, false),
                }
            };

        let mut http_request = http::Request::builder().uri(req.uri()).method(req.method());
        *http_request.headers_mut().unwrap() = req.headers().clone();

        if let Some(CanonicalJsonValue::Object(json_body)) = &mut json_body {
            let user_id = sender_user.clone().unwrap_or_else(|| {
                UserId::parse_with_server_name("", db.globals.server_name())
                    .expect("we know this is valid")
            });

            let uiaa_request = json_body
                .get("auth")
                .and_then(|auth| auth.as_object())
                .and_then(|auth| auth.get("session"))
                .and_then(|session| session.as_str())
                .and_then(|session| {
                    db.uiaa.get_uiaa_request(
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
            warn!("{:?}", e);
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
            "HeaderValue to decode should start with \"X-Matrix ..\", received = {:?}",
            value,
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
                _ => warn!(
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
