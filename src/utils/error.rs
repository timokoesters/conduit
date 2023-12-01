use std::convert::Infallible;

use http::StatusCode;
use ruma::{
    api::client::{
        error::{Error as RumaError, ErrorBody, ErrorKind},
        uiaa::{UiaaInfo, UiaaResponse},
    },
    OwnedServerName,
};
use thiserror::Error;
use tracing::{error, info};

#[cfg(feature = "persy")]
use persy::PersyError;

use crate::RumaResponse;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Error, Debug)]
pub enum Error {
    #[cfg(feature = "sled")]
    #[error("There was a problem with the connection to the sled database.")]
    SledError {
        #[from]
        source: sled::Error,
    },
    #[cfg(feature = "sqlite")]
    #[error("There was a problem with the connection to the sqlite database: {source}")]
    SqliteError {
        #[from]
        source: rusqlite::Error,
    },
    #[cfg(feature = "persy")]
    #[error("There was a problem with the connection to the persy database.")]
    PersyError { source: PersyError },
    #[cfg(feature = "heed")]
    #[error("There was a problem with the connection to the heed database: {error}")]
    HeedError { error: String },
    #[cfg(feature = "rocksdb")]
    #[error("There was a problem with the connection to the rocksdb database: {source}")]
    RocksDbError {
        #[from]
        source: rocksdb::Error,
    },
    #[error("Could not generate an image.")]
    ImageError {
        #[from]
        source: image::error::ImageError,
    },
    #[error("Could not connect to server: {source}")]
    ReqwestError {
        #[from]
        source: reqwest::Error,
    },
    #[error("{0}")]
    FederationError(OwnedServerName, RumaError),
    #[error("Could not do this io: {source}")]
    IoError {
        #[from]
        source: std::io::Error,
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
    #[cfg(feature = "conduit_bin")]
    #[error("{0}")]
    ExtensionError(#[from] axum::extract::rejection::ExtensionRejection),
    #[cfg(feature = "conduit_bin")]
    #[error("{0}")]
    PathError(#[from] axum::extract::rejection::PathRejection),
    #[error("from {0}: {1}")]
    RedactionError(OwnedServerName, ruma::canonical_json::RedactionError),
}

impl Error {
    pub fn bad_database(message: &'static str) -> Self {
        error!("BadDatabase: {}", message);
        Self::BadDatabase(message)
    }

    pub fn bad_config(message: &'static str) -> Self {
        error!("BadConfig: {}", message);
        Self::BadConfig(message)
    }
}

impl Error {
    pub fn to_response(&self) -> RumaResponse<UiaaResponse> {
        if let Self::Uiaa(uiaainfo) = self {
            return RumaResponse(UiaaResponse::AuthResponse(uiaainfo.clone()));
        }

        if let Self::FederationError(origin, error) = self {
            let mut error = error.clone();
            error.body = ErrorBody::Standard {
                kind: Unknown,
                message: format!("Answer from {origin}: {error}"),
            };
            return RumaResponse(UiaaResponse::MatrixError(error));
        }

        let message = format!("{self}");

        use ErrorKind::*;
        let (kind, status_code) = match self {
            Self::BadRequest(kind, _) => (
                kind.clone(),
                match kind {
                    Forbidden | GuestAccessForbidden | ThreepidAuthFailed | ThreepidDenied => {
                        StatusCode::FORBIDDEN
                    }
                    Unauthorized | UnknownToken { .. } | MissingToken => StatusCode::UNAUTHORIZED,
                    NotFound | Unrecognized => StatusCode::NOT_FOUND,
                    LimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
                    UserDeactivated => StatusCode::FORBIDDEN,
                    TooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                    _ => StatusCode::BAD_REQUEST,
                },
            ),
            Self::Conflict(_) => (Unknown, StatusCode::CONFLICT),
            _ => (Unknown, StatusCode::INTERNAL_SERVER_ERROR),
        };

        info!("Returning an error: {}: {}", status_code, message);

        RumaResponse(UiaaResponse::MatrixError(RumaError {
            body: ErrorBody::Standard { kind, message },
            status_code,
        }))
    }

    /// Sanitizes public-facing errors that can leak sensitive information.
    pub fn sanitized_error(&self) -> String {
        let db_error = String::from("Database or I/O error occurred.");

        match self {
            #[cfg(feature = "sled")]
            Self::SledError { .. } => db_error,
            #[cfg(feature = "sqlite")]
            Self::SqliteError { .. } => db_error,
            #[cfg(feature = "persy")]
            Self::PersyError { .. } => db_error,
            #[cfg(feature = "heed")]
            Self::HeedError => db_error,
            #[cfg(feature = "rocksdb")]
            Self::RocksDbError { .. } => db_error,
            Self::IoError { .. } => db_error,
            Self::BadConfig { .. } => db_error,
            Self::BadDatabase { .. } => db_error,
            _ => self.to_string(),
        }
    }
}

#[cfg(feature = "persy")]
impl<T: Into<PersyError>> From<persy::PE<T>> for Error {
    fn from(err: persy::PE<T>) -> Self {
        Error::PersyError {
            source: err.error().into(),
        }
    }
}

impl From<Infallible> for Error {
    fn from(i: Infallible) -> Self {
        match i {}
    }
}

#[cfg(feature = "conduit_bin")]
impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        self.to_response().into_response()
    }
}
