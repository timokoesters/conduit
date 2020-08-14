use log::error;
use ruma::api::client::{error::ErrorKind, r0::uiaa::UiaaInfo};
use thiserror::Error;

#[cfg(feature = "conduit_bin")]
use {
    crate::RumaResponse,
    http::StatusCode,
    rocket::{
        response::{self, Responder},
        Request,
    },
    ruma::api::client::{error::Error as RumaError, r0::uiaa::UiaaResponse},
};

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("There was a problem with the connection to the database.")]
    SledError {
        #[from]
        source: sled::Error,
    },
    #[error("Could not generate an image.")]
    ImageError {
        #[from]
        source: image::error::ImageError,
    },
    #[error("Could not connect to server.")]
    ReqwestError {
        #[from]
        source: reqwest::Error,
    },
    #[error("{0}")]
    BadServerResponse(&'static str),
    #[error("{0}")]
    BadConfig(&'static str),
    #[error("{0}")]
    /// Don't create this directly. Use Error::bad_database instead.
    BadDatabase(&'static str),
    #[error("uiaa")]
    Uiaa(UiaaInfo),

    #[error("{0}: {1}")]
    BadRequest(ErrorKind, &'static str),
    #[error("{0}")]
    Conflict(&'static str), // This is only needed for when a room alias already exists
}

impl Error {
    pub fn bad_database(message: &'static str) -> Self {
        error!("BadDatabase: {}", message);
        Self::BadDatabase(message)
    }
}

#[cfg(feature = "conduit_bin")]
impl<'r, 'o> Responder<'r, 'o> for Error
where
    'o: 'r,
{
    fn respond_to(self, r: &'r Request<'_>) -> response::Result<'o> {
        if let Self::Uiaa(uiaainfo) = &self {
            return RumaResponse::from(UiaaResponse::AuthResponse(uiaainfo.clone())).respond_to(r);
        }

        let message = format!("{}", self);

        use ErrorKind::*;
        let (kind, status_code) = match self {
            Self::BadRequest(kind, _) => (
                kind,
                match kind {
                    Forbidden | GuestAccessForbidden | ThreepidAuthFailed | ThreepidDenied => {
                        StatusCode::FORBIDDEN
                    }
                    Unauthorized | UnknownToken | MissingToken => StatusCode::UNAUTHORIZED,
                    NotFound => StatusCode::NOT_FOUND,
                    LimitExceeded => StatusCode::TOO_MANY_REQUESTS,
                    UserDeactivated => StatusCode::FORBIDDEN,
                    TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                    _ => StatusCode::BAD_REQUEST,
                },
            ),
            Self::Conflict(_) => (Unknown, StatusCode::CONFLICT),
            _ => (Unknown, StatusCode::INTERNAL_SERVER_ERROR),
        };

        RumaResponse::from(RumaError {
            kind,
            message,
            status_code,
        })
        .respond_to(r)
    }
}
