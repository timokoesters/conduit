use std::{collections::HashMap, num::NonZeroU64};

use bytesize::ByteSize;
use serde::Deserialize;

use crate::service::rate_limiting::{ClientRestriction, FederationRestriction, Restriction};

#[derive(Debug, Clone, Deserialize)]
pub struct WrappedShadowConfig {
    #[serde(default)]
    pub inherits: ConfigPreset,
    #[serde(flatten)]
    pub config: ShadowConfig,
}

impl From<WrappedShadowConfig> for Config {
    fn from(value: WrappedShadowConfig) -> Self {
        Config::get_preset(value.inherits).apply_overrides(value.config)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ConfigPreset {
    /// Default rate-limiting configuration, recommended for small private servers (i.e. single-user
    /// or for family and/or friends)
    #[default]
    PrivateSmall,
    PrivateMedium,
    PublicMedium,
    PublicLarge,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShadowConfig {
    pub client:
        ShadowConfigFragment<ClientRestriction, ShadowClientMediaConfig, AuthenticationFailures>,
    pub federation:
        ShadowConfigFragment<FederationRestriction, ShadowFederationMediaConfig, Nothing>,
}

pub trait RestrictionGeneric: ConfigPart + std::hash::Hash + Eq {}
impl<T> RestrictionGeneric for T where T: ConfigPart + std::hash::Hash + Eq {}

pub trait ConfigPart: Clone + std::fmt::Debug + serde::de::DeserializeOwned {}
impl<T> ConfigPart for T where T: Clone + std::fmt::Debug + serde::de::DeserializeOwned {}

#[derive(Debug, Clone, Deserialize)]
pub struct ShadowConfigFragment<R, M, T>
where
    R: RestrictionGeneric,
    M: ConfigPart,
    T: ConfigPart,
{
    #[serde(bound(deserialize = "R: RestrictionGeneric, M: ConfigPart, T: ConfigPart"))]
    pub target: Option<ShadowConfigFragmentFragment<R, M, T>>,
    #[serde(bound(deserialize = "R: RestrictionGeneric, M: ConfigPart"))]
    pub global: Option<ShadowConfigFragmentFragment<R, M, Nothing>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShadowConfigFragmentFragment<R, M, T>
where
    R: RestrictionGeneric,
    M: ConfigPart,
    T: ConfigPart,
{
    #[serde(
        flatten,
        // https://play.rust-lang.org/?version=stable&mode=debug&edition=2024&gist=fe75063b73c6d9860991c41572e00035
        // 
        // For some reason specifying the default function fixes the issue in the playground link
        // above. Makes no sense to me, but hey, it works.
        default = "HashMap::new",
        bound(deserialize = "R: RestrictionGeneric")
    )]
    pub map: HashMap<R, RequestLimitation>,
    #[serde(bound(deserialize = "M: ConfigPart"))]
    pub media: Option<M>,
    #[serde(flatten)]
    #[serde(bound(deserialize = "T: ConfigPart"))]
    pub additional_fields: Option<T>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ShadowClientMediaConfig {
    pub download: Option<MediaLimitation>,
    pub upload: Option<MediaLimitation>,
    pub fetch: Option<MediaLimitation>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ShadowFederationMediaConfig {
    pub download: Option<MediaLimitation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(from = "WrappedShadowConfig")]
pub struct Config {
    pub target: ConfigFragment<AuthenticationFailures>,
    pub global: ConfigFragment<Nothing>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigFragment<T>
where
    T: ConfigPart,
{
    #[serde(bound(deserialize = "T: ConfigPart"))]
    pub client: ConfigFragmentFragment<ClientRestriction, ClientMediaConfig, T>,
    pub federation: ConfigFragmentFragment<FederationRestriction, FederationMediaConfig, Nothing>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigFragmentFragment<R, M, T>
where
    R: RestrictionGeneric,
    M: ConfigPart,
    T: ConfigPart,
{
    #[serde(flatten)]
    #[serde(bound(deserialize = "R: RestrictionGeneric"))]
    pub map: HashMap<R, RequestLimitation>,
    #[serde(bound(deserialize = "M: ConfigPart"))]
    pub media: M,
    #[serde(flatten)]
    #[serde(bound(deserialize = "T: ConfigPart"))]
    pub additional_fields: T,
}

impl<R, M, T> ConfigFragmentFragment<R, M, T>
where
    R: RestrictionGeneric,
    M: ConfigPart + MediaConfig,
    T: ConfigPart,
{
    pub fn apply_overrides(
        self,
        shadow: Option<ShadowConfigFragmentFragment<R, M::Shadow, T>>,
    ) -> Self {
        let Some(shadow) = shadow else {
            return self;
        };

        let ConfigFragmentFragment {
            map: mut map,
            media,
            additional_fields,
        } = self;

        map.extend(shadow.map);

        Self {
            map,
            media: if let Some(sm) = shadow.media {
                media.apply_overrides(sm)
            } else {
                media
            },
            additional_fields: shadow.additional_fields.unwrap_or(additional_fields),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuthenticationFailures {
    pub authentication_failures: RequestLimitation,
}

impl AuthenticationFailures {
    fn new(timeframe: Timeframe, burst_capacity: NonZeroU64) -> Self {
        Self {
            authentication_failures: RequestLimitation::new(timeframe, burst_capacity),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Nothing;

impl<T> ConfigFragment<T>
where
    T: ConfigPart,
{
    pub fn get(&self, restriction: &Restriction) -> &RequestLimitation {
        // Maybe look into https://github.com/moriyoshi-kasuga/enum-table
        match restriction {
            Restriction::Client(client_restriction) => {
                self.client.map.get(client_restriction).unwrap()
            }
            Restriction::Federation(federation_restriction) => {
                self.federation.map.get(federation_restriction).unwrap()
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct RequestLimitation {
    #[serde(flatten)]
    pub timeframe: Timeframe,
    pub burst_capacity: NonZeroU64,
}

impl RequestLimitation {
    pub fn new(timeframe: Timeframe, burst_capacity: NonZeroU64) -> Self {
        Self {
            timeframe,
            burst_capacity,
        }
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
// When deserializing, we want this prefix
#[allow(clippy::enum_variant_names)]
pub enum Timeframe {
    PerSecond(NonZeroU64),
    PerMinute(NonZeroU64),
    PerHour(NonZeroU64),
    PerDay(NonZeroU64),
}

impl Timeframe {
    pub fn nano_gap(&self) -> u64 {
        match self {
            Timeframe::PerSecond(t) => 1000 * 1000 * 1000 / t.get(),
            Timeframe::PerMinute(t) => 1000 * 1000 * 1000 * 60 / t.get(),
            Timeframe::PerHour(t) => 1000 * 1000 * 1000 * 60 * 60 / t.get(),
            Timeframe::PerDay(t) => 1000 * 1000 * 1000 * 60 * 60 * 24 / t.get(),
        }
    }
}

pub trait MediaConfig {
    type Shadow: ConfigPart;

    fn apply_overrides(self, shadow: Self::Shadow) -> Self;
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ClientMediaConfig {
    pub download: MediaLimitation,
    pub upload: MediaLimitation,
    pub fetch: MediaLimitation,
}

impl MediaConfig for ClientMediaConfig {
    type Shadow = ShadowClientMediaConfig;

    fn apply_overrides(self, shadow: Self::Shadow) -> Self {
        let Self::Shadow {
            download,
            upload,
            fetch,
        } = shadow;

        Self {
            download: download.unwrap_or(self.download),
            upload: upload.unwrap_or(self.upload),
            fetch: fetch.unwrap_or(self.fetch),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FederationMediaConfig {
    pub download: MediaLimitation,
}

impl MediaConfig for FederationMediaConfig {
    type Shadow = ShadowFederationMediaConfig;

    fn apply_overrides(self, shadow: Self::Shadow) -> Self {
        Self {
            download: shadow.download.unwrap_or(self.download),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct MediaLimitation {
    #[serde(flatten)]
    pub timeframe: MediaTimeframe,
    pub burst_capacity: ByteSize,
}

impl MediaLimitation {
    pub fn new(timeframe: MediaTimeframe, burst_capacity: ByteSize) -> Self {
        Self {
            timeframe,
            burst_capacity,
        }
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
// When deserializing, we want this prefix
#[allow(clippy::enum_variant_names)]
pub enum MediaTimeframe {
    PerSecond(ByteSize),
    PerMinute(ByteSize),
    PerHour(ByteSize),
    PerDay(ByteSize),
}

impl MediaTimeframe {
    pub fn bytes_per_sec(&self) -> u64 {
        match self {
            MediaTimeframe::PerSecond(t) => t.as_u64(),
            MediaTimeframe::PerMinute(t) => t.as_u64() / 60,
            MediaTimeframe::PerHour(t) => t.as_u64() / (60 * 60),
            MediaTimeframe::PerDay(t) => t.as_u64() / (60 * 60 * 24),
        }
    }
}

fn nz(int: u64) -> NonZeroU64 {
    NonZeroU64::new(int).expect("Values are static")
}

macro_rules! default_restriction_map {
    ($restriction_type:ident; $($restriction:ident, $timeframe:ident, $timeframe_value:expr, $burst_capacity:expr);*) => {
            HashMap::from_iter([
                $((
                    $restriction_type::$restriction,
                    RequestLimitation::new(Timeframe::$timeframe(nz($timeframe_value)), nz($burst_capacity)),
                ),)*
            ])
    }
}

impl Config {
    fn apply_overrides(self, shadow: ShadowConfig) -> Self {
        let ShadowConfig {
            client:
                ShadowConfigFragment {
                    target: client_target,
                    global: client_global,
                },
            federation:
                ShadowConfigFragment {
                    target: federation_target,
                    global: federation_global,
                },
        } = shadow;

        Self {
            target: ConfigFragment {
                client: self.target.client.apply_overrides(client_target),
                federation: self.target.federation.apply_overrides(federation_target),
            },
            global: ConfigFragment {
                client: self.global.client.apply_overrides(client_global),
                federation: self.global.federation.apply_overrides(federation_global),
            },
        }
    }

    fn get_preset(preset: ConfigPreset) -> Self {
        match preset {
            //TODO: finish these
            ConfigPreset::PrivateSmall => Self {
                target: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            ClientRestriction;
                            // Registration, PerDay, 10, 20;
                            // Login, PerHour, 10, 10;
                            // RegistrationTokenValidity, PerDay, 10, 20
                        ),
                        media: ClientMediaConfig {
                            download: todo!(),
                            upload: todo!(),
                            fetch: todo!(),
                        },
                        additional_fields: AuthenticationFailures::new(
                            Timeframe::PerDay(nz(10)),
                            nz(40),
                        ),
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;
                            Join, PerDay, 10, 20;
                            Knock, PerDay, 10, 20;
                            Invite, PerDay, 10, 20
                        ),
                        media: todo!(),
                        additional_fields: Nothing,
                    },
                },
                global: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            ClientRestriction;
                            Registration, PerDay, 10, 20;
                            Login, PerHour, 10, 10;
                            RegistrationTokenValidity, PerDay, 10, 20
                        ),
                        media: todo!(),
                        additional_fields: Nothing,
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;
                            // Join, PerDay, 10, 20;
                            // Knock, PerDay, 10, 20;
                            // Invite, PerDay, 10, 20
                        ),
                        media: todo!(),
                        additional_fields: Nothing,
                    },
                },
            },
            ConfigPreset::PrivateMedium => todo!(),
            ConfigPreset::PublicMedium => todo!(),
            ConfigPreset::PublicLarge => todo!(),
        }
    }
}
