use std::{collections::HashMap, num::NonZeroU64};

use bytesize::ByteSize;
use serde::Deserialize;

use crate::service::rate_limiting::{ClientRestriction, FederationRestriction, Restriction};

#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    #[serde(flatten)]
    pub target: ConfigFragment,
    pub global: ConfigFragment,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigFragment {
    pub client: ConfigClientFragment,
    pub federation: ConfigFederationFragment,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigClientFragment {
    pub map: HashMap<ClientRestriction, RequestLimitation>,
    pub media: ClientMediaConfig,
    // TODO: Only have available on target, not global (same with most authenticated endpoints too maybe)?
    pub authentication_failures: RequestLimitation,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigFederationFragment {
    pub map: HashMap<FederationRestriction, RequestLimitation>,
    pub media: FederationMediaConfig,
}

impl ConfigFragment {
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

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ClientMediaConfig {
    pub download: MediaLimitation,
    pub upload: MediaLimitation,
    pub fetch: MediaLimitation,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct FederationMediaConfig {
    pub download: MediaLimitation,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct MediaLimitation {
    #[serde(flatten)]
    pub timeframe: MediaTimeframe,
    pub burst_capacity: ByteSize,
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
