use std::{
    collections::BTreeMap,
    fmt,
    net::{IpAddr, Ipv4Addr},
};

use ruma::{OwnedServerName, RoomVersionId};
use serde::{de::IgnoredAny, Deserialize};
use tracing::warn;
use url::Url;

mod proxy;

use self::proxy::ProxyConfig;

#[derive(Clone, Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_address")]
    pub address: IpAddr,
    #[serde(default = "default_port")]
    pub port: u16,
    pub tls: Option<TlsConfig>,

    pub server_name: OwnedServerName,
    pub database_backend: String,
    pub database_path: String,
    #[serde(default = "default_db_cache_capacity_mb")]
    pub db_cache_capacity_mb: f64,
    #[serde(default = "true_fn")]
    pub enable_lightning_bolt: bool,
    #[serde(default = "true_fn")]
    pub allow_check_for_updates: bool,
    #[serde(default = "default_conduit_cache_capacity_modifier")]
    pub conduit_cache_capacity_modifier: f64,
    #[serde(default = "default_rocksdb_max_open_files")]
    pub rocksdb_max_open_files: i32,
    #[serde(default = "default_pdu_cache_capacity")]
    pub pdu_cache_capacity: u32,
    #[serde(default = "default_cleanup_second_interval")]
    pub cleanup_second_interval: u32,
    #[serde(default = "default_max_request_size")]
    pub max_request_size: u32,
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: u16,
    #[serde(default = "default_max_fetch_prev_events")]
    pub max_fetch_prev_events: u16,
    #[serde(default = "false_fn")]
    pub allow_registration: bool,
    pub registration_token: Option<String>,
    #[serde(default = "default_openid_token_ttl")]
    pub openid_token_ttl: u64,
    #[serde(default = "true_fn")]
    pub allow_encryption: bool,
    #[serde(default = "false_fn")]
    pub allow_federation: bool,
    #[serde(default = "true_fn")]
    pub allow_room_creation: bool,
    #[serde(default = "true_fn")]
    pub allow_unstable_room_versions: bool,
    #[serde(default = "default_default_room_version")]
    pub default_room_version: RoomVersionId,
    #[serde(default)]
    pub well_known: WellKnownConfig,
    #[serde(default = "false_fn")]
    pub allow_jaeger: bool,
    #[serde(default = "false_fn")]
    pub tracing_flame: bool,
    #[serde(default)]
    pub proxy: ProxyConfig,
    pub jwt_secret: Option<String>,
    #[serde(default = "default_trusted_servers")]
    pub trusted_servers: Vec<OwnedServerName>,
    #[serde(default = "default_log")]
    pub log: String,
    #[serde(default)]
    pub turn_username: String,
    #[serde(default)]
    pub turn_password: String,
    #[serde(default = "Vec::new")]
    pub turn_uris: Vec<String>,
    #[serde(default)]
    pub turn_secret: String,
    #[serde(default = "default_turn_ttl")]
    pub turn_ttl: u64,

    pub emergency_password: Option<String>,

    #[serde(flatten)]
    pub catchall: BTreeMap<String, IgnoredAny>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TlsConfig {
    pub certs: String,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct WellKnownConfig {
    pub client: Option<Url>,
    pub server: Option<OwnedServerName>,
}

const DEPRECATED_KEYS: &[&str] = &["cache_capacity"];

impl Config {
    pub fn warn_deprecated(&self) {
        let mut was_deprecated = false;
        for key in self
            .catchall
            .keys()
            .filter(|key| DEPRECATED_KEYS.iter().any(|s| s == key))
        {
            warn!("Config parameter {} is deprecated", key);
            was_deprecated = true;
        }

        if was_deprecated {
            warn!("Read conduit documentation and check your configuration if any new configuration parameters should be adjusted");
        }
    }
}

impl Config {
    pub fn well_known_client(&self) -> String {
        if let Some(url) = &self.well_known.client {
            url.to_string()
        } else {
            format!("https://{}", self.server_name)
        }
    }

    pub fn well_known_server(&self) -> OwnedServerName {
        match &self.well_known.server {
            Some(server_name) => server_name.to_owned(),
            None => {
                if self.server_name.port().is_some() {
                    self.server_name.to_owned()
                } else {
                    format!("{}:443", self.server_name.host())
                        .try_into()
                        .expect("Host from valid hostname + :443 must be valid")
                }
            }
        }
    }
}

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Prepare a list of config values to show
        let well_known_server = self.well_known_server();
        let lines = [
            ("Server name", self.server_name.host()),
            ("Database backend", &self.database_backend),
            ("Database path", &self.database_path),
            (
                "Database cache capacity (MB)",
                &self.db_cache_capacity_mb.to_string(),
            ),
            (
                "Cache capacity modifier",
                &self.conduit_cache_capacity_modifier.to_string(),
            ),
            #[cfg(feature = "rocksdb")]
            (
                "Maximum open files for RocksDB",
                &self.rocksdb_max_open_files.to_string(),
            ),
            ("PDU cache capacity", &self.pdu_cache_capacity.to_string()),
            (
                "Cleanup interval in seconds",
                &self.cleanup_second_interval.to_string(),
            ),
            ("Maximum request size", &self.max_request_size.to_string()),
            (
                "Maximum concurrent requests",
                &self.max_concurrent_requests.to_string(),
            ),
            ("Allow registration", &self.allow_registration.to_string()),
            (
                "Enabled lightning bolt",
                &self.enable_lightning_bolt.to_string(),
            ),
            ("Allow encryption", &self.allow_encryption.to_string()),
            ("Allow federation", &self.allow_federation.to_string()),
            ("Allow room creation", &self.allow_room_creation.to_string()),
            (
                "JWT secret",
                match self.jwt_secret {
                    Some(_) => "set",
                    None => "not set",
                },
            ),
            ("Trusted servers", {
                let mut lst = vec![];
                for server in &self.trusted_servers {
                    lst.push(server.host());
                }
                &lst.join(", ")
            }),
            (
                "TURN username",
                if self.turn_username.is_empty() {
                    "not set"
                } else {
                    &self.turn_username
                },
            ),
            ("TURN password", {
                if self.turn_password.is_empty() {
                    "not set"
                } else {
                    "set"
                }
            }),
            ("TURN secret", {
                if self.turn_secret.is_empty() {
                    "not set"
                } else {
                    "set"
                }
            }),
            ("Turn TTL", &self.turn_ttl.to_string()),
            ("Turn URIs", {
                let mut lst = vec![];
                for item in self.turn_uris.iter().cloned().enumerate() {
                    let (_, uri): (usize, String) = item;
                    lst.push(uri);
                }
                &lst.join(", ")
            }),
            ("Well-known server name", well_known_server.as_str()),
            ("Well-known client URL", &self.well_known_client()),
        ];

        let mut msg: String = "Active config values:\n\n".to_owned();

        for line in lines.into_iter().enumerate() {
            msg += &format!("{}: {}\n", line.1 .0, line.1 .1);
        }

        write!(f, "{msg}")
    }
}

fn false_fn() -> bool {
    false
}

fn true_fn() -> bool {
    true
}

fn default_address() -> IpAddr {
    Ipv4Addr::LOCALHOST.into()
}

fn default_port() -> u16 {
    8000
}

fn default_db_cache_capacity_mb() -> f64 {
    300.0
}

fn default_conduit_cache_capacity_modifier() -> f64 {
    1.0
}

fn default_rocksdb_max_open_files() -> i32 {
    1000
}

fn default_pdu_cache_capacity() -> u32 {
    150_000
}

fn default_cleanup_second_interval() -> u32 {
    60 // every minute
}

fn default_max_request_size() -> u32 {
    20 * 1024 * 1024 // Default to 20 MB
}

fn default_max_concurrent_requests() -> u16 {
    100
}

fn default_max_fetch_prev_events() -> u16 {
    100_u16
}

fn default_trusted_servers() -> Vec<OwnedServerName> {
    vec![OwnedServerName::try_from("matrix.org").unwrap()]
}

fn default_log() -> String {
    "warn,state_res=warn,_=off".to_owned()
}

fn default_turn_ttl() -> u64 {
    60 * 60 * 24
}

fn default_openid_token_ttl() -> u64 {
    60 * 60
}

// I know, it's a great name
pub fn default_default_room_version() -> RoomVersionId {
    RoomVersionId::V10
}
