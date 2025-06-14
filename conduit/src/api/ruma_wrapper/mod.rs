use crate::{Error, service::appservice::RegistrationInfo};
use ruma::{
    CanonicalJsonValue, OwnedDeviceId, OwnedServerName, OwnedUserId,
    api::client::uiaa::UiaaResponse,
};
use std::{net::IpAddr, ops::Deref};

#[cfg(feature = "conduit_bin")]
mod axum;

/// Extractor for Ruma request structs
pub struct Ruma<T> {
    pub body: T,
    pub sender_user: Option<OwnedUserId>,
    pub sender_device: Option<OwnedDeviceId>,
    pub sender_servername: Option<OwnedServerName>,
    pub sender_ip_address: Option<IpAddr>,
    // This is None when body is not a valid string
    pub json_body: Option<CanonicalJsonValue>,
    pub appservice_info: Option<RegistrationInfo>,
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
