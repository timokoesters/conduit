use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fmt,
    net::{IpAddr, Ipv4Addr},
    num::NonZeroU8,
    path::PathBuf,
    time::Duration,
};

use bytesize::ByteSize;
use ruma::{OwnedServerName, RoomVersionId};
use serde::{de::IgnoredAny, Deserialize};
use tokio::time::{interval, Interval};
use tracing::warn;
use url::Url;

use crate::Error;

mod proxy;
use self::proxy::ProxyConfig;

const SHA256_HEX_LENGTH: u8 = 64;

#[derive(Deserialize)]
pub struct IncompleteConfig {
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
    pub well_known: IncompleteWellKnownConfig,
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
    pub turn_username: Option<String>,
    pub turn_password: Option<String>,
    pub turn_uris: Option<Vec<String>>,
    pub turn_secret: Option<String>,
    #[serde(default = "default_turn_ttl")]
    pub turn_ttl: u64,

    pub turn: Option<TurnConfig>,

    #[serde(default)]
    pub media: IncompleteMediaConfig,

    pub emergency_password: Option<String>,

    #[serde(flatten)]
    pub catchall: BTreeMap<String, IgnoredAny>,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(from = "IncompleteConfig")]
pub struct Config {
    pub address: IpAddr,
    pub port: u16,
    pub tls: Option<TlsConfig>,

    pub server_name: OwnedServerName,
    pub database_backend: String,
    pub database_path: String,
    pub db_cache_capacity_mb: f64,
    pub enable_lightning_bolt: bool,
    pub allow_check_for_updates: bool,
    pub conduit_cache_capacity_modifier: f64,
    pub rocksdb_max_open_files: i32,
    pub pdu_cache_capacity: u32,
    pub cleanup_second_interval: u32,
    pub max_request_size: u32,
    pub max_concurrent_requests: u16,
    pub max_fetch_prev_events: u16,
    pub allow_registration: bool,
    pub registration_token: Option<String>,
    pub openid_token_ttl: u64,
    pub allow_encryption: bool,
    pub allow_federation: bool,
    pub allow_room_creation: bool,
    pub allow_unstable_room_versions: bool,
    pub default_room_version: RoomVersionId,
    pub well_known: WellKnownConfig,
    pub allow_jaeger: bool,
    pub tracing_flame: bool,
    pub proxy: ProxyConfig,
    pub jwt_secret: Option<String>,
    pub trusted_servers: Vec<OwnedServerName>,
    pub log: String,

    pub turn: Option<TurnConfig>,

    pub media: MediaConfig,

    pub emergency_password: Option<String>,

    pub catchall: BTreeMap<String, IgnoredAny>,
}

impl From<IncompleteConfig> for Config {
    fn from(val: IncompleteConfig) -> Self {
        let IncompleteConfig {
            address,
            port,
            tls,
            server_name,
            database_backend,
            database_path,
            db_cache_capacity_mb,
            enable_lightning_bolt,
            allow_check_for_updates,
            conduit_cache_capacity_modifier,
            rocksdb_max_open_files,
            pdu_cache_capacity,
            cleanup_second_interval,
            max_request_size,
            max_concurrent_requests,
            max_fetch_prev_events,
            allow_registration,
            registration_token,
            openid_token_ttl,
            allow_encryption,
            allow_federation,
            allow_room_creation,
            allow_unstable_room_versions,
            default_room_version,
            well_known,
            allow_jaeger,
            tracing_flame,
            proxy,
            jwt_secret,
            trusted_servers,
            log,
            turn_username,
            turn_password,
            turn_uris,
            turn_secret,
            turn_ttl,
            turn,
            media,
            emergency_password,
            catchall,
        } = val;

        let turn = turn.or_else(|| {
            let auth = if let Some(secret) = turn_secret {
                TurnAuth::Secret { secret }
            } else if let (Some(username), Some(password)) = (turn_username, turn_password) {
                TurnAuth::UserPass { username, password }
            } else {
                return None;
            };

            if let (Some(uris), ttl) = (turn_uris, turn_ttl) {
                Some(TurnConfig { uris, ttl, auth })
            } else {
                None
            }
        });

        let well_known_client = well_known
            .client
            .map(String::from)
            .unwrap_or_else(|| format!("https://{server_name}"));

        let well_known_server = well_known.server.unwrap_or_else(|| {
            if server_name.port().is_some() {
                server_name.clone()
            } else {
                format!("{}:443", server_name.host())
                    .try_into()
                    .expect("Host from valid hostname + :443 must be valid")
            }
        });

        let well_known = WellKnownConfig {
            client: well_known_client,
            server: well_known_server,
        };

        let media = MediaConfig {
            backend: match media.backend {
                IncompleteMediaBackendConfig::FileSystem {
                    path,
                    directory_structure,
                } => MediaBackendConfig::FileSystem {
                    path: path.unwrap_or_else(|| {
                        // We do this as we don't know if the path has a trailing slash, or even if the
                        // path separator is a forward or backward slash
                        [&database_path, "media"]
                            .iter()
                            .collect::<PathBuf>()
                            .into_os_string()
                            .into_string()
                            .expect("Both inputs are valid UTF-8")
                    }),
                    directory_structure,
                },
                IncompleteMediaBackendConfig::S3(value) => MediaBackendConfig::S3 {
                    bucket: value.bucket,
                    credentials: value.credentials,
                    duration: value.duration,
                    path: value.path,
                    directory_structure: value.directory_structure,
                },
            },
            retention: media.retention.into(),
        };

        Config {
            address,
            port,
            tls,
            server_name,
            database_backend,
            database_path,
            db_cache_capacity_mb,
            enable_lightning_bolt,
            allow_check_for_updates,
            conduit_cache_capacity_modifier,
            rocksdb_max_open_files,
            pdu_cache_capacity,
            cleanup_second_interval,
            max_request_size,
            max_concurrent_requests,
            max_fetch_prev_events,
            allow_registration,
            registration_token,
            openid_token_ttl,
            allow_encryption,
            allow_federation,
            allow_room_creation,
            allow_unstable_room_versions,
            default_room_version,
            well_known,
            allow_jaeger,
            tracing_flame,
            proxy,
            jwt_secret,
            trusted_servers,
            log,
            turn,
            media,
            emergency_password,
            catchall,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct TlsConfig {
    pub certs: String,
    pub key: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct TurnConfig {
    pub uris: Vec<String>,
    #[serde(default = "default_turn_ttl")]
    pub ttl: u64,
    #[serde(flatten)]
    pub auth: TurnAuth,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum TurnAuth {
    UserPass { username: String, password: String },
    Secret { secret: String },
}

#[derive(Clone, Debug, Deserialize, Default)]
pub struct IncompleteWellKnownConfig {
    // We use URL here so that the user gets an error if the config isn't a valid url
    pub client: Option<Url>,
    pub server: Option<OwnedServerName>,
}

#[derive(Clone, Debug)]
pub struct WellKnownConfig {
    // We use String here as there is no point converting our manually constructed String into a
    // URL, just for it to be converted back into a &str
    pub client: String,
    pub server: OwnedServerName,
}

#[derive(Deserialize, Default)]
pub struct IncompleteMediaConfig {
    #[serde(flatten, default)]
    pub backend: IncompleteMediaBackendConfig,
    pub retention: IncompleteMediaRetentionConfig,
}

#[derive(Clone, Debug)]
pub struct MediaConfig {
    pub backend: MediaBackendConfig,
    pub retention: MediaRetentionConfig,
}

type IncompleteMediaRetentionConfig = Option<HashSet<IncompleteScopedMediaRetentionConfig>>;

#[derive(Clone, Debug)]
pub struct MediaRetentionConfig {
    pub scoped: HashMap<MediaRetentionScope, ScopedMediaRetentionConfig>,
    pub global_space: Option<ByteSize>,
}

impl MediaRetentionConfig {
    /// Interval for the duration-based retention policies to be checked & enforced
    pub fn cleanup_interval(&self) -> Option<Interval> {
        self.scoped
            .values()
            .filter_map(|scoped| match (scoped.created, scoped.accessed) {
                (None, accessed) => accessed,
                (created, None) => created,
                (created, accessed) => created.min(accessed),
            })
            .map(|dur| {
                dur.mul_f32(0.1)
                    .max(Duration::from_secs(60).min(Duration::from_secs(60 * 60 * 24)))
            })
            .min()
            .map(interval)
    }
}

#[derive(Deserialize)]
pub struct IncompleteScopedMediaRetentionConfig {
    pub scope: Option<MediaRetentionScope>,
    #[serde(default, with = "humantime_serde::option")]
    pub accessed: Option<Duration>,
    #[serde(default, with = "humantime_serde::option")]
    pub created: Option<Duration>,
    pub space: Option<ByteSize>,
}

impl From<IncompleteMediaRetentionConfig> for MediaRetentionConfig {
    fn from(value: IncompleteMediaRetentionConfig) -> Self {
        {
            let mut scoped = HashMap::from([
                (
                    MediaRetentionScope::Remote,
                    ScopedMediaRetentionConfig::default(),
                ),
                (
                    MediaRetentionScope::Thumbnail,
                    ScopedMediaRetentionConfig::default(),
                ),
            ]);
            let mut fallback = None;

            if let Some(retention) = value {
                for IncompleteScopedMediaRetentionConfig {
                    scope,
                    accessed,
                    space,
                    created,
                } in retention
                {
                    if let Some(scope) = scope {
                        scoped.insert(
                            scope,
                            ScopedMediaRetentionConfig {
                                accessed,
                                space,
                                created,
                            },
                        );
                    } else {
                        fallback = Some(ScopedMediaRetentionConfig {
                            accessed,
                            space,
                            created,
                        })
                    }
                }
            }

            if let Some(fallback) = fallback.clone() {
                for scope in [
                    MediaRetentionScope::Remote,
                    MediaRetentionScope::Local,
                    MediaRetentionScope::Thumbnail,
                ] {
                    scoped.entry(scope).or_insert_with(|| fallback.clone());
                }
            }

            Self {
                global_space: fallback.and_then(|global| global.space),
                scoped,
            }
        }
    }
}

impl std::hash::Hash for IncompleteScopedMediaRetentionConfig {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.scope.hash(state);
    }
}

impl PartialEq for IncompleteScopedMediaRetentionConfig {
    fn eq(&self, other: &Self) -> bool {
        self.scope == other.scope
    }
}

impl Eq for IncompleteScopedMediaRetentionConfig {}

#[derive(Debug, Clone)]
pub struct ScopedMediaRetentionConfig {
    pub accessed: Option<Duration>,
    pub created: Option<Duration>,
    pub space: Option<ByteSize>,
}

impl Default for ScopedMediaRetentionConfig {
    fn default() -> Self {
        Self {
            // 30 days
            accessed: Some(Duration::from_secs(60 * 60 * 24 * 30)),
            created: None,
            space: None,
        }
    }
}

#[derive(Deserialize, Clone, Debug, Hash, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MediaRetentionScope {
    Remote,
    Local,
    Thumbnail,
}

#[derive(Deserialize)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum IncompleteMediaBackendConfig {
    FileSystem {
        path: Option<String>,
        #[serde(default)]
        directory_structure: DirectoryStructure,
    },
    S3(S3MediaBackend),
}

impl Default for IncompleteMediaBackendConfig {
    fn default() -> Self {
        Self::FileSystem {
            path: None,
            directory_structure: DirectoryStructure::default(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum MediaBackendConfig {
    FileSystem {
        path: String,
        directory_structure: DirectoryStructure,
    },
    S3 {
        bucket: Box<rusty_s3::Bucket>,
        credentials: Box<rusty_s3::Credentials>,
        duration: Duration,
        path: Option<String>,
        directory_structure: DirectoryStructure,
    },
}

#[derive(Debug, Clone, Deserialize)]
// See https://github.com/serde-rs/serde/issues/642#issuecomment-525432907
#[serde(try_from = "ShadowDirectoryStructure", untagged)]
pub enum DirectoryStructure {
    // We do this enum instead of Option<DirectoryStructure>, so that we can have the structure be
    // deep by default, while still providing a away for it to be flat (by creating an empty table)
    //
    // e.g.:
    // ```toml
    // [global.media.directory_structure]
    // ```
    Flat,
    Deep { length: NonZeroU8, depth: NonZeroU8 },
}

impl Default for DirectoryStructure {
    fn default() -> Self {
        Self::Deep {
            length: NonZeroU8::new(2).expect("2 is not 0"),
            depth: NonZeroU8::new(2).expect("2 is not 0"),
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ShadowDirectoryStructure {
    Deep { length: NonZeroU8, depth: NonZeroU8 },
    Flat {},
}

impl TryFrom<ShadowDirectoryStructure> for DirectoryStructure {
    type Error = Error;

    fn try_from(value: ShadowDirectoryStructure) -> Result<Self, Self::Error> {
        match value {
            ShadowDirectoryStructure::Flat {} => Ok(Self::Flat),
            ShadowDirectoryStructure::Deep { length, depth } => {
                if length
                    .get()
                    .checked_mul(depth.get())
                    .map(|product| product < SHA256_HEX_LENGTH)
                    // If an overflow occurs, it definitely isn't less than SHA256_HEX_LENGTH
                    .unwrap_or(false)
                {
                    Ok(Self::Deep { length, depth })
                } else {
                    Err(Error::bad_config("The media directory structure depth multiplied by the depth is equal to or greater than a sha256 hex hash, please reduce at least one of the two so that their product is less than 64"))
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct ShadowS3MediaBackend {
    endpoint: Url,
    bucket: String,
    region: String,
    path: Option<String>,

    key: String,
    secret: String,

    #[serde(default = "default_s3_duration")]
    duration: u64,
    #[serde(default = "false_fn")]
    bucket_use_path: bool,
    #[serde(default)]
    directory_structure: DirectoryStructure,
}

impl TryFrom<ShadowS3MediaBackend> for S3MediaBackend {
    type Error = Error;

    fn try_from(value: ShadowS3MediaBackend) -> Result<Self, Self::Error> {
        let path_style = if value.bucket_use_path {
            rusty_s3::UrlStyle::Path
        } else {
            rusty_s3::UrlStyle::VirtualHost
        };
        let credentials = rusty_s3::Credentials::new(value.key, value.secret);

        match rusty_s3::Bucket::new(value.endpoint, path_style, value.bucket, value.region) {
            Ok(bucket) => Ok(S3MediaBackend {
                bucket: Box::new(bucket),
                credentials: Box::new(credentials),
                duration: Duration::from_secs(value.duration),
                path: value.path,
                directory_structure: value.directory_structure,
            }),
            Err(_) => Err(Error::bad_config("Invalid S3 config")),
        }
    }
}

#[derive(Deserialize, Debug, Clone)]
#[serde(try_from = "ShadowS3MediaBackend")]
pub struct S3MediaBackend {
    pub bucket: Box<rusty_s3::Bucket>,
    pub credentials: Box<rusty_s3::Credentials>,
    pub duration: Duration,
    pub path: Option<String>,
    pub directory_structure: DirectoryStructure,
}

const DEPRECATED_KEYS: &[&str] = &[
    "cache_capacity",
    "turn_username",
    "turn_password",
    "turn_uris",
    "turn_secret",
    "turn_ttl",
];

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

impl fmt::Display for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Prepare a list of config values to show
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
            ("TURN URIs", {
                if let Some(turn) = &self.turn {
                    let mut lst = vec![];
                    for item in turn.uris.iter().cloned().enumerate() {
                        let (_, uri): (usize, String) = item;
                        lst.push(uri);
                    }
                    &lst.join(", ")
                } else {
                    "unset"
                }
            }),
            ("Well-known server name", self.well_known.server.as_str()),
            ("Well-known client URL", &self.well_known.client),
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

fn default_s3_duration() -> u64 {
    30
}
