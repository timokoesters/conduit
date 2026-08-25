use std::{
    collections::BTreeMap,
    error::Error as _,
    iter::FromIterator,
    net::{IpAddr, SocketAddr},
    ops::Deref,
    str::{self, FromStr},
};

use axum::{
    RequestPartsExt,
    body::Body,
    extract::{ConnectInfo, FromRequest, Path},
    response::{IntoResponse, Response},
};
use bytes::{BufMut, Bytes, BytesMut};
use conduit_config::IpAddrDetection;
use http::{Request, StatusCode, request::Parts};
use ruma::{
    CanonicalJsonValue, MilliSecondsSinceUnixEpoch, OwnedServerName, UserId,
    api::{
        IncomingRequestExt, OutgoingResponseExt,
        auth_scheme::{
            AccessToken, AccessTokenOptional, AppserviceToken, AppserviceTokenOptional, AuthScheme,
            NoAccessToken, NoAuthentication,
        },
        error::{ErrorKind, UnknownTokenErrorData},
        federation::authentication::ServerSignatures,
    },
};
use serde::Deserialize;
use tracing::{debug, error, warn};

use super::{Ruma, RumaResponse};
use crate::{
    Error, Result,
    api::{OptionalAuthSenderInfo, UserOrAppserviceInfo},
    service::appservice::RegistrationInfo,
    services,
};

async fn into_checked_parts(
    req: Request<Body>,
) -> Result<(Parts, Bytes, Option<CanonicalJsonValue>)> {
    let (parts, body) = req.into_parts();
    let body = axum::body::to_bytes(
        body,
        services()
            .globals
            .max_request_size()
            .try_into()
            .unwrap_or(usize::MAX),
    )
    .await
    .map_err(|err| {
        if err
            .source()
            .is_some_and(|err| err.is::<http_body_util::LengthLimitError>())
        {
            Error::BadRequest(ErrorKind::TooLarge, "Reached maximum request size")
        } else {
            error!("An unknown error has occurred: {err}");
            Error::BadRequest(ErrorKind::Unknown, "An unknown error has occurred")
        }
    })?;

    let json_body = serde_json::from_slice::<CanonicalJsonValue>(&body).ok();

    Ok((parts, body, json_body))
}

async fn extract_ip_address(parts: &mut Parts) -> Result<Option<IpAddr>> {
    Ok(match &services().globals.config.ip_address_detection {
        IpAddrDetection::SocketAddress => {
            let addr: ConnectInfo<SocketAddr> = parts.extract().await?;
            Some(addr.ip())
        }
        IpAddrDetection::Header(name) => parts
            .headers
            .get(name)
            .and_then(|header| header.to_str().ok())
            .map(|header| header.split_once(',').map(|(ip, _)| ip).unwrap_or(header))
            .and_then(|ip| IpAddr::from_str(ip).ok()),
    })
}

pub trait SupportedAuthScheme: AuthScheme {
    type SenderInfo;

    /// Takes the extracted authentication from the auth scheme, and checks that it's valid.
    #[allow(clippy::type_complexity)]
    fn process_auth(
        req: Request<Body>,
        extracted_auth: <Self as AuthScheme>::Output,
    ) -> impl Future<
        Output = Result<(
            Parts,
            Bytes,
            Option<CanonicalJsonValue>,
            Option<Self::SenderInfo>,
        )>,
    > + Send;
}

impl SupportedAuthScheme for ServerSignatures {
    type SenderInfo = OwnedServerName;

    async fn process_auth(
        req: Request<Body>,
        extracted_auth: <Self as AuthScheme>::Output,
    ) -> Result<(
        Parts,
        Bytes,
        Option<CanonicalJsonValue>,
        Option<Self::SenderInfo>,
    )> {
        let (parts, body, json_body) = into_checked_parts(req).await?;

        let keys_result = services()
            .rooms
            .event_handler
            .fetch_signing_keys(
                &extracted_auth.origin,
                vec![extracted_auth.key.to_string()],
                false,
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

        // Only verify_keys that are currently valid should be used for validating requests
        // as per MSC4029
        let pub_key_map = BTreeMap::from_iter([(
            extracted_auth.origin.as_str().to_owned(),
            if keys.valid_until_ts > MilliSecondsSinceUnixEpoch::now() {
                keys.verify_keys
                    .into_iter()
                    .map(|(id, key)| (id, key.key))
                    .collect()
            } else {
                BTreeMap::new()
            },
        )]);

        let mut req = Request::builder().uri(parts.uri).method(parts.method);
        *req.headers_mut().unwrap() = parts.headers;
        let req = req.body(body).unwrap();
        debug!("{:?}", req);
        match extracted_auth.verify_http_request(
            &req,
            services().globals.server_name(),
            &pub_key_map,
        ) {
            Ok(()) => {
                let (parts, body) = req.into_parts();

                Ok((parts, body, json_body, Some(extracted_auth.origin)))
            }
            Err(e) => {
                warn!(
                    "Failed to verify json request from {}: {e}",
                    extracted_auth.origin
                );

                if req.uri().to_string().contains('@') {
                    warn!(
                        "Request uri contained '@' character. Make sure your \
                                         reverse proxy gives Conduit the raw uri (apache: use \
                                         nocanon)"
                    );
                }

                Err(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "Failed to verify X-Matrix signatures.",
                ))
            }
        }
    }
}

async fn unknown_access_token(ip_addr: Option<IpAddr>) -> Error {
    if let Some(addr) = ip_addr {
        services()
            .rate_limiting
            .update_post_auth_failure(addr)
            .await;
    } else {
        error!(
            "Auth failure occurred, but IP address was not extracted. Please check your Conduit & reverse proxy configuration, as if nothing is done, an attacker can brute-force access tokens and login to user's accounts"
        );
    }

    Error::BadRequest(
        ErrorKind::UnknownToken(UnknownTokenErrorData { soft_logout: false }),
        "Unknown access token.",
    )
}

impl SupportedAuthScheme for AccessToken {
    type SenderInfo = UserOrAppserviceInfo;

    async fn process_auth(
        req: Request<Body>,
        extracted_auth: <Self as AuthScheme>::Output,
    ) -> Result<(
        Parts,
        Bytes,
        Option<CanonicalJsonValue>,
        Option<Self::SenderInfo>,
    )> {
        let (mut parts, mut body, mut json_body) = into_checked_parts(req).await?;

        let ip_addr = extract_ip_address(&mut parts).await?;

        if let Some(ip_addr) = ip_addr {
            services().rate_limiting.pre_auth_check(ip_addr).await?;
        }

        if let Some(info) = services().appservice.find_from_token(&extracted_auth).await {
            #[derive(Deserialize)]
            struct QueryParams {
                user_id: Option<String>,
                // TODO: device ID assertion
                #[allow(dead_code)]
                device_id: Option<String>,
            }

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
                    ErrorKind::Forbidden,
                    "User does not exist.",
                ));
            };

            Ok((
                parts,
                body,
                json_body,
                Some(Self::SenderInfo {
                    user_id,
                    // TODO: device ID assertion
                    device_id: None,
                    appservice_info: Some(info),
                }),
            ))
        } else if let Some((user_id, device_id)) =
            services().users.find_from_token(&extracted_auth)?
        {
            if let Some(CanonicalJsonValue::Object(json_body)) = &mut json_body {
                let uiaa_request = json_body
                    .get("auth")
                    .and_then(|auth| auth.as_object())
                    .and_then(|auth| auth.get("session"))
                    .and_then(|session| session.as_str())
                    .and_then(|session| {
                        services()
                            .uiaa
                            .get_uiaa_request(&user_id, &device_id, session)
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

            Ok((
                parts,
                body,
                json_body,
                Some(Self::SenderInfo {
                    user_id,
                    device_id: Some(device_id),
                    appservice_info: None,
                }),
            ))
        } else {
            Err(unknown_access_token(ip_addr).await)
        }
    }
}

impl SupportedAuthScheme for AccessTokenOptional {
    type SenderInfo = OptionalAuthSenderInfo<UserOrAppserviceInfo>;

    async fn process_auth(
        req: Request<Body>,
        extracted_auth: <Self as AuthScheme>::Output,
    ) -> Result<(
        Parts,
        Bytes,
        Option<CanonicalJsonValue>,
        Option<Self::SenderInfo>,
    )> {
        if let Some(token) = extracted_auth {
            AccessToken::process_auth(req, token)
                .await
                .map(|(req, path_params, json_body, out)| {
                    (
                        req,
                        path_params,
                        json_body,
                        out.map(OptionalAuthSenderInfo::Some),
                    )
                })
        } else {
            let (mut parts, body, json_body) = into_checked_parts(req).await?;
            let ip_addr = extract_ip_address(&mut parts).await?;
            Ok((
                parts,
                body,
                json_body,
                ip_addr.map(OptionalAuthSenderInfo::None),
            ))
        }
    }
}

impl SupportedAuthScheme for AppserviceToken {
    type SenderInfo = RegistrationInfo;

    async fn process_auth(
        req: Request<Body>,
        extracted_auth: <Self as AuthScheme>::Output,
    ) -> Result<(
        Parts,
        Bytes,
        Option<CanonicalJsonValue>,
        Option<Self::SenderInfo>,
    )> {
        let (mut parts, body, json_body) = into_checked_parts(req).await?;
        let ip_addr = extract_ip_address(&mut parts).await?;

        if let Some(ip_addr) = ip_addr {
            services().rate_limiting.pre_auth_check(ip_addr).await?;
        }

        if let Some(info) = services().appservice.find_from_token(&extracted_auth).await {
            Ok((parts, body, json_body, Some(info)))
        } else {
            Err(unknown_access_token(ip_addr).await)
        }
    }
}

impl SupportedAuthScheme for AppserviceTokenOptional {
    type SenderInfo = OptionalAuthSenderInfo<RegistrationInfo>;

    async fn process_auth(
        req: Request<Body>,
        extracted_auth: <Self as AuthScheme>::Output,
    ) -> Result<(
        Parts,
        Bytes,
        Option<CanonicalJsonValue>,
        Option<Self::SenderInfo>,
    )> {
        if let Some(token) = extracted_auth {
            AppserviceToken::process_auth(req, token).await.map(
                |(req, path_params, json_body, out)| {
                    (
                        req,
                        path_params,
                        json_body,
                        out.map(OptionalAuthSenderInfo::Some),
                    )
                },
            )
        } else {
            let (mut parts, body, json_body) = into_checked_parts(req).await?;
            let ip_addr = extract_ip_address(&mut parts).await?;
            Ok((
                parts,
                body,
                json_body,
                ip_addr.map(OptionalAuthSenderInfo::None),
            ))
        }
    }
}

impl SupportedAuthScheme for NoAccessToken {
    type SenderInfo = IpAddr;

    async fn process_auth(
        req: Request<Body>,
        _extracted_auth: <Self as AuthScheme>::Output,
    ) -> Result<(
        Parts,
        Bytes,
        Option<CanonicalJsonValue>,
        Option<Self::SenderInfo>,
    )> {
        let (mut parts, body, json_body) = into_checked_parts(req).await?;
        let ip_addr = extract_ip_address(&mut parts).await?;
        Ok((parts, body, json_body, ip_addr))
    }
}

impl SupportedAuthScheme for NoAuthentication {
    type SenderInfo = IpAddr;

    async fn process_auth(
        req: Request<Body>,
        _extracted_auth: <Self as AuthScheme>::Output,
    ) -> Result<(
        Parts,
        Bytes,
        Option<CanonicalJsonValue>,
        Option<Self::SenderInfo>,
    )> {
        let (mut parts, body, json_body) = into_checked_parts(req).await?;
        let ip_addr = extract_ip_address(&mut parts).await?;
        Ok((parts, body, json_body, ip_addr))
    }
}

impl<T, S> FromRequest<S> for Ruma<T>
where
    T: IncomingRequestExt,
    T::Authentication: SupportedAuthScheme,
    <T::Authentication as SupportedAuthScheme>::SenderInfo: Send,
    S: Sync,
{
    type Rejection = Error;

    async fn from_request(req: Request<Body>, _state: &S) -> Result<Self, Self::Rejection> {
        let Ok(authentication) = T::Authentication::extract_authentication(&req) else {
            return Err(Error::BadRequest(
                ErrorKind::Unauthorized,
                "Failed to authenticate request",
            ));
        };
        let (mut parts, body, json_body, sender) =
            <T::Authentication as SupportedAuthScheme>::process_auth(req, authentication).await?;
        let Some(sender) = sender else {
            error!(
                "No sender info from request, make sure IP address extraction is configured correctly.
                See https://docs.conduit.rs/configuration.html#ip-address-detection on how to do that."
            );
            return Err(Error::BadServerResponse(
                "No sender info was extracted from request",
            ));
        };

        let path_params: Path<Vec<String>> = parts.extract().await?;

        let mut http_request = Request::builder().uri(parts.uri).method(parts.method);
        *http_request.headers_mut().unwrap() = parts.headers;

        let http_request = http_request.body(body.as_ref()).unwrap();

        debug!("{:?}", http_request);

        let body = T::try_from_http_request(
            http_request,
            &path_params
                .deref()
                .iter()
                .map(|x| x.as_str())
                .collect::<Vec<_>>(),
        )
        .map_err(|e| {
            warn!("try_from_http_request failed: {:?}", e);
            debug!("JSON body: {:?}", json_body);
            Error::BadRequest(ErrorKind::BadJson, "Failed to deserialize request.")
        })?;

        Ok(Ruma {
            body,
            sender,
            json_body,
        })
    }
}

impl<T: OutgoingResponseExt> IntoResponse for RumaResponse<T> {
    fn into_response(self) -> Response {
        match self.0.try_into_http_response::<BytesMut>() {
            Ok(res) => res.map(BytesMut::freeze).map(Body::from).into_response(),
            Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
}
