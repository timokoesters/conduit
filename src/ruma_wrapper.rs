use crate::Error;
use ruma::{
    api::{AuthScheme, IncomingRequest, OutgoingRequest},
    identifiers::{DeviceId, UserId},
    Outgoing,
};
use std::{
    convert::{TryFrom, TryInto},
    ops::Deref,
};

#[cfg(feature = "conduit_bin")]
use {
    crate::utils,
    log::{debug, warn},
    rocket::{
        data::{
            ByteUnit, Data, FromDataFuture, FromTransformedData, Transform, TransformFuture,
            Transformed,
        },
        http::Status,
        outcome::Outcome::*,
        response::{self, Responder},
        tokio::io::AsyncReadExt,
        Request, State,
    },
    std::io::Cursor,
};

/// This struct converts rocket requests into ruma structs by converting them into http requests
/// first.
pub struct Ruma<T: Outgoing + OutgoingRequest> {
    pub body: T::Incoming,
    pub sender_user: Option<UserId>,
    pub sender_device: Option<Box<DeviceId>>,
    pub json_body: Option<Box<serde_json::value::RawValue>>, // This is None when body is not a valid string
    pub from_appservice: bool,
}

#[cfg(feature = "conduit_bin")]
impl<'a, T: Outgoing + OutgoingRequest> FromTransformedData<'a> for Ruma<T>
where
    T::Incoming: IncomingRequest,
{
    type Error = ();
    type Owned = Data;
    type Borrowed = Self::Owned;

    fn transform<'r>(
        _req: &'r Request<'_>,
        data: Data,
    ) -> TransformFuture<'r, Self::Owned, Self::Error> {
        Box::pin(async move { Transform::Owned(Success(data)) })
    }

    fn from_data(
        request: &'a Request<'_>,
        outcome: Transformed<'a, Self>,
    ) -> FromDataFuture<'a, Self, Self::Error> {
        Box::pin(async move {
            let data = rocket::try_outcome!(outcome.owned());
            let db = request
                .guard::<State<'_, crate::Database>>()
                .await
                .expect("database was loaded");

            // Get token from header or query value
            let token = request
                .headers()
                .get_one("Authorization")
                .map(|s| s[7..].to_owned()) // Split off "Bearer "
                .or_else(|| request.get_query_value("access_token").and_then(|r| r.ok()));

            let (sender_user, sender_device, from_appservice) = if let Some((_id, registration)) =
                db.appservice
                    .iter_all()
                    .filter_map(|r| r.ok())
                    .find(|(_id, registration)| {
                        registration
                            .get("as_token")
                            .and_then(|as_token| as_token.as_str())
                            .map_or(false, |as_token| {
                                dbg!(token.as_deref()) == dbg!(Some(as_token))
                            })
                    }) {
                match T::METADATA.authentication {
                    AuthScheme::AccessToken | AuthScheme::QueryOnlyAccessToken => {
                        let user_id = request.get_query_value::<String>("user_id").map_or_else(
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
                            |string| {
                                UserId::try_from(string.expect("parsing to string always works"))
                                    .unwrap()
                            },
                        );

                        if !db.users.exists(&user_id).unwrap() {
                            // Forbidden
                            return Failure((Status::raw(580), ()));
                        }

                        // TODO: Check if appservice is allowed to be that user
                        (Some(user_id), None, true)
                    }
                    AuthScheme::ServerSignatures => (None, None, true),
                    AuthScheme::None => (None, None, true),
                }
            } else {
                match T::METADATA.authentication {
                    AuthScheme::AccessToken | AuthScheme::QueryOnlyAccessToken => {
                        if let Some(token) = token {
                            match db.users.find_from_token(&token).unwrap() {
                                // Unknown Token
                                None => return Failure((Status::raw(581), ())),
                                Some((user_id, device_id)) => {
                                    (Some(user_id), Some(device_id.into()), false)
                                }
                            }
                        } else {
                            // Missing Token
                            return Failure((Status::raw(582), ()));
                        }
                    }
                    AuthScheme::ServerSignatures => (None, None, false),
                    AuthScheme::None => (None, None, false),
                }
            };

            let mut http_request = http::Request::builder()
                .uri(request.uri().to_string())
                .method(&*request.method().to_string());
            for header in request.headers().iter() {
                http_request = http_request.header(header.name.as_str(), &*header.value);
            }

            let limit = db.globals.max_request_size();
            let mut handle = data.open(ByteUnit::Byte(limit.into()));
            let mut body = Vec::new();
            handle.read_to_end(&mut body).await.unwrap();

            let http_request = http_request.body(body.clone()).unwrap();
            debug!("{:?}", http_request);
            match <T::Incoming as IncomingRequest>::try_from_http_request(http_request) {
                Ok(t) => Success(Ruma {
                    body: t,
                    sender_user,
                    sender_device,
                    // TODO: Can we avoid parsing it again? (We only need this for append_pdu)
                    json_body: utils::string_from_bytes(&body)
                        .ok()
                        .and_then(|s| serde_json::value::RawValue::from_string(s).ok()),
                    from_appservice,
                }),
                Err(e) => {
                    warn!("{:?}", e);
                    Failure((Status::raw(583), ()))
                }
            }
        })
    }
}

impl<T: Outgoing + OutgoingRequest> Deref for Ruma<T> {
    type Target = T::Incoming;

    fn deref(&self) -> &Self::Target {
        &self.body
    }
}

/// This struct converts ruma responses into rocket http responses.
pub type ConduitResult<T> = std::result::Result<RumaResponse<T>, Error>;

pub struct RumaResponse<T: TryInto<http::Response<Vec<u8>>>>(pub T);

impl<T: TryInto<http::Response<Vec<u8>>>> From<T> for RumaResponse<T> {
    fn from(t: T) -> Self {
        Self(t)
    }
}

#[cfg(feature = "conduit_bin")]
impl<'r, 'o, T> Responder<'r, 'o> for RumaResponse<T>
where
    T: Send + TryInto<http::Response<Vec<u8>>>,
    T::Error: Send,
    'o: 'r,
{
    fn respond_to(self, _: &'r Request<'_>) -> response::Result<'o> {
        let http_response: Result<http::Response<_>, _> = self.0.try_into();
        match http_response {
            Ok(http_response) => {
                let mut response = rocket::response::Response::build();

                let status = http_response.status();
                response.raw_status(status.into(), "");

                for header in http_response.headers() {
                    response
                        .raw_header(header.0.to_string(), header.1.to_str().unwrap().to_owned());
                }

                let http_body = http_response.into_body();

                response.sized_body(http_body.len(), Cursor::new(http_body));

                response.raw_header("Access-Control-Allow-Origin", "*");
                response.raw_header(
                    "Access-Control-Allow-Methods",
                    "GET, POST, PUT, DELETE, OPTIONS",
                );
                response.raw_header(
                    "Access-Control-Allow-Headers",
                    "Origin, X-Requested-With, Content-Type, Accept, Authorization",
                );
                response.ok()
            }
            Err(_) => Err(Status::InternalServerError),
        }
    }
}
