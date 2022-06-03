use crate::Error;
use ruma::{
    api::client::uiaa::UiaaResponse, signatures::CanonicalJsonValue, OwnedDeviceId,
    OwnedServerName, OwnedUserId,
};
use std::ops::Deref;

#[cfg(feature = "conduit_bin")]
mod axum;

/// Extractor for Ruma request structs
pub struct Ruma<T> {
    pub body: T,
    pub sender_user: Option<OwnedUserId>,
    pub sender_device: Option<OwnedDeviceId>,
    pub sender_servername: Option<OwnedServerName>,
    // This is None when body is not a valid string
    pub json_body: Option<CanonicalJsonValue>,
    pub from_appservice: bool,
}

impl<T> Deref for Ruma<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.body
    }
}

#[derive(Clone)]
pub struct RumaResponse<T>(pub T);

impl<T> From<T> for RumaResponse<T> {
    fn from(t: T) -> Self {
        Self(t)
    }
}

impl From<Error> for RumaResponse<UiaaResponse> {
    fn from(t: Error) -> Self {
        t.to_response()
    }
}
