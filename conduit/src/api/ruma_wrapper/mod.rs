use crate::{Error, api::ruma_wrapper::axum::SupportedAuthScheme};
use ruma::{
    CanonicalJsonValue,
    api::{IncomingRequest, client::uiaa::UiaaResponse},
};
use std::ops::Deref;

#[cfg(feature = "conduit_bin")]
pub mod axum;

/// Extractor for Ruma request structs
pub struct Ruma<T>
where
    T: IncomingRequest,
    T::Authentication: SupportedAuthScheme,
{
    pub body: T,
    pub sender: <T::Authentication as SupportedAuthScheme>::SenderInfo,
    // This is None when body is not a valid string
    pub json_body: Option<CanonicalJsonValue>,
}

impl<T> Deref for Ruma<T>
where
    T: IncomingRequest,
    T::Authentication: SupportedAuthScheme,
{
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
