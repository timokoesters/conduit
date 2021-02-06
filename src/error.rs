use std::{
    collections::HashMap,
    sync::RwLock,
    time::{Duration, Instant},
};

use log::error;
use ruma::{
    api::client::{error::ErrorKind, r0::uiaa::UiaaInfo},
    events::room::message,
};
use thiserror::Error;

use crate::{database::admin::AdminCommand, Database};

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
    #[error("Could not connect to server: {source}")]
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

    pub fn bad_config(message: &'static str) -> Self {
        error!("BadConfig: {}", message);
        Self::BadConfig(message)
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
                kind.clone(),
                match kind {
                    Forbidden | GuestAccessForbidden | ThreepidAuthFailed | ThreepidDenied => {
                        StatusCode::FORBIDDEN
                    }
                    Unauthorized | UnknownToken { .. } | MissingToken => StatusCode::UNAUTHORIZED,
                    NotFound => StatusCode::NOT_FOUND,
                    LimitExceeded { .. } => StatusCode::TOO_MANY_REQUESTS,
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

pub struct ConduitLogger {
    pub db: Database,
    pub last_logs: RwLock<HashMap<String, Instant>>,
}

impl log::Log for ConduitLogger {
    fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
        true
    }

    fn log(&self, record: &log::Record<'_>) {
        let output = format!("{} - {}", record.level(), record.args());

        let match_mod_path =
            |path: &str| path.starts_with("conduit::") || path.starts_with("state");

        if self.enabled(record.metadata())
            && (record.module_path().map_or(false, match_mod_path)
                || record
                    .module_path()
                    .map_or(true, |path| !path.starts_with("rocket::")) // Rockets logs are annoying
                    && record.metadata().level() <= log::Level::Warn)
        {
            let first_line = output
                .lines()
                .next()
                .expect("lines always returns one item");

            eprintln!("{}", output);

            let mute_duration = match record.metadata().level() {
                log::Level::Error => Duration::from_secs(60 * 5), // 5 minutes
                log::Level::Warn => Duration::from_secs(60 * 60 * 24), // A day
                _ => Duration::from_secs(60 * 60 * 24 * 7),       // A week
            };

            if self
                .last_logs
                .read()
                .unwrap()
                .get(first_line)
                .map_or(false, |i| i.elapsed() < mute_duration)
            // Don't post this log again for some time
            {
                return;
            }

            if let Ok(mut_last_logs) = &mut self.last_logs.try_write() {
                mut_last_logs.insert(first_line.to_owned(), Instant::now());
            }

            self.db.admin.send(AdminCommand::SendMessage(
                message::MessageEventContent::notice_plain(output),
            ));
        }
    }

    fn flush(&self) {}
}
