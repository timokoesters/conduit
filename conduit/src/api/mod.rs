use std::{borrow::Cow, collections::BTreeSet, net::IpAddr, sync::LazyLock};

use ruma::{
    OwnedDeviceId, OwnedUserId, ServerName,
    api::{
        MatrixVersion, SupportedVersions,
        auth_scheme::{AuthScheme, NoAccessToken, NoAuthentication, SendAccessToken},
        federation::authentication::{ServerSignatures, XMatrixSigningInput},
        path_builder::{PathBuilder, SinglePath, VersionHistory},
    },
};

use crate::{service::appservice::RegistrationInfo, services};

pub mod appservice_server;
pub mod client_server;
pub mod ruma_wrapper;
pub mod server_server;

pub trait SupportedPathBuilder: PathBuilder {
    fn input() -> <Self as PathBuilder>::Input<'static>;
}

impl SupportedPathBuilder for SinglePath {
    fn input() -> Self::Input<'static> {}
}

static SUPPORTED_VERSIONS: LazyLock<SupportedVersions> = LazyLock::new(|| SupportedVersions {
    versions: BTreeSet::from_iter([MatrixVersion::V1_13]),
    features: BTreeSet::new(),
});

impl SupportedPathBuilder for VersionHistory {
    fn input() -> Self::Input<'static> {
        Cow::Borrowed(&*SUPPORTED_VERSIONS)
    }
}

#[derive(Clone)]
pub enum OptionalAuthSenderInfo<T> {
    Some(T),
    None(IpAddr),
}

impl<T> OptionalAuthSenderInfo<T> {
    pub fn is_some(&self) -> bool {
        match self {
            OptionalAuthSenderInfo::Some(_) => true,
            OptionalAuthSenderInfo::None(_) => false,
        }
    }

    pub fn is_none(&self) -> bool {
        match self {
            OptionalAuthSenderInfo::Some(_) => false,
            OptionalAuthSenderInfo::None(_) => true,
        }
    }
}

#[derive(Clone)]
pub struct UserOrAppserviceInfo {
    pub user_id: OwnedUserId,
    pub device_id: Option<OwnedDeviceId>,
    pub appservice_info: Option<RegistrationInfo>,
}

pub trait FederationAuthScheme<'a>: AuthScheme {
    fn input(destination: &ServerName) -> <Self as AuthScheme>::Input<'a>;
}

impl<'a> FederationAuthScheme<'a> for ServerSignatures {
    fn input(destination: &ServerName) -> <Self as AuthScheme>::Input<'a> {
        XMatrixSigningInput::new(
            services().globals.server_name().to_owned(),
            destination.to_owned(),
            services().globals.keypair(),
        )
    }
}

impl<'a> FederationAuthScheme<'a> for NoAuthentication {
    fn input(_destination: &ServerName) -> <Self as AuthScheme>::Input<'a> {}
}

// For non-authenticated media endpoints
impl<'a> FederationAuthScheme<'a> for NoAccessToken {
    fn input(_destination: &ServerName) -> <Self as AuthScheme>::Input<'a> {
        SendAccessToken::None
    }
}
