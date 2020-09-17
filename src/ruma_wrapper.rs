use crate::Error;
use ruma::{
    api::{Outgoing, OutgoingRequest},
    identifiers::{DeviceId, UserId},
};
use std::{
    convert::{TryFrom, TryInto},
    ops::Deref,
};

#[cfg(feature = "conduit_bin")]
use {
    crate::utils,
    log::warn,
    rocket::{
        data::{
            Data, FromDataFuture, FromTransformedData, Transform, TransformFuture, Transformed,
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
pub struct Ruma<T: Outgoing> {
    pub body: T::Incoming,
    pub sender_id: Option<UserId>,
    pub device_id: Option<Box<DeviceId>>,
    pub json_body: Option<Box<serde_json::value::RawValue>>, // This is None when body is not a valid string
}

#[cfg(feature = "conduit_bin")]
impl<'a, T: Outgoing + OutgoingRequest> FromTransformedData<'a> for Ruma<T>
where
    <T as Outgoing>::Incoming: TryFrom<http::request::Request<std::vec::Vec<u8>>> + std::fmt::Debug,
    <<T as Outgoing>::Incoming as std::convert::TryFrom<
        http::request::Request<std::vec::Vec<u8>>,
    >>::Error: std::fmt::Debug,
{
    type Error = (); // TODO: Better error handling
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

            let (user_id, device_id) = if T::METADATA.requires_authentication {
                // Get token from header or query value
                let token = match request
                    .headers()
                    .get_one("Authorization")
                    .map(|s| s[7..].to_owned()) // Split off "Bearer "
                    .or_else(|| request.get_query_value("access_token").and_then(|r| r.ok()))
                {
                    // TODO: M_MISSING_TOKEN
                    None => return Failure((Status::Unauthorized, ())),
                    Some(token) => token,
                };

                // Check if token is valid
                match db.users.find_from_token(&token).unwrap() {
                    // TODO: M_UNKNOWN_TOKEN
                    None => return Failure((Status::Unauthorized, ())),
                    Some((user_id, device_id)) => (Some(user_id), Some(device_id.into())),
                }
            } else {
                (None, None)
            };

            let mut http_request = http::Request::builder()
                .uri(request.uri().to_string())
                .method(&*request.method().to_string());
            for header in request.headers().iter() {
                http_request = http_request.header(header.name.as_str(), &*header.value);
            }

            let limit = db.globals.max_request_size();
            let mut handle = data.open().take(limit.into());
            let mut body = Vec::new();
            handle.read_to_end(&mut body).await.unwrap();

            let http_request = http_request.body(body.clone()).unwrap();
            log::info!("{:?}", http_request);

            match <T as Outgoing>::Incoming::try_from(http_request) {
                Ok(t) => Success(Ruma {
                    body: t,
                    sender_id: user_id,
                    device_id,
                    // TODO: Can we avoid parsing it again? (We only need this for append_pdu)
                    json_body: utils::string_from_bytes(&body)
                        .ok()
                        .and_then(|s| serde_json::value::RawValue::from_string(s).ok()),
                }),
                Err(e) => {
                    warn!("{:?}", e);
                    Failure((Status::BadRequest, ()))
                }
            }
        })
    }
}

impl<T: Outgoing> Deref for Ruma<T> {
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
