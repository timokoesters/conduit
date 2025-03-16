mod data;
pub use data::{Data, SigningKeys};
use ruma::{
    serde::Base64, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedEventId, OwnedRoomAliasId,
    OwnedRoomId, OwnedServerName, OwnedUserId, RoomAliasId,
};

use crate::api::server_server::DestinationResponse;

use crate::{
    config::{MediaConfig, TurnConfig},
    services, Config, Error, Result,
};
use futures_util::FutureExt;
use hickory_resolver::TokioAsyncResolver;
use hyper_util::client::legacy::connect::dns::{GaiResolver, Name as HyperName};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};
use ruma::{
    api::{client::sync::sync_events, federation::discovery::ServerSigningKeys},
    DeviceId, RoomVersionId, ServerName, UserId,
};
use std::{
    collections::{BTreeMap, HashMap},
    error::Error as StdError,
    fs,
    future::{self, Future},
    iter,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    str::FromStr,
    sync::{
        atomic::{self, AtomicBool},
        Arc, RwLock as StdRwLock,
    },
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, watch::Receiver, Mutex, RwLock, Semaphore};
use tower_service::Service as TowerService;
use tracing::{error, info};

type WellKnownMap = HashMap<OwnedServerName, DestinationResponse>;
type TlsNameMap = HashMap<String, (Vec<IpAddr>, u16)>;
type RateLimitState = (Instant, u32); // Time if last failed try, number of failed tries
type SyncHandle = (
    Option<String>,                                      // since
    Receiver<Option<Result<sync_events::v3::Response>>>, // rx
);

pub struct Service {
    pub db: &'static dyn Data,

    pub actual_destination_cache: Arc<RwLock<WellKnownMap>>, // actual_destination, host
    pub tls_name_override: Arc<StdRwLock<TlsNameMap>>,
    pub config: Config,
    allow_registration: RwLock<bool>,
    keypair: Arc<ruma::signatures::Ed25519KeyPair>,
    dns_resolver: TokioAsyncResolver,
    jwt_decoding_key: Option<jsonwebtoken::DecodingKey>,
    federation_client: reqwest::Client,
    default_client: reqwest::Client,
    pub stable_room_versions: Vec<RoomVersionId>,
    pub unstable_room_versions: Vec<RoomVersionId>,
    pub bad_event_ratelimiter: Arc<RwLock<HashMap<OwnedEventId, RateLimitState>>>,
    pub bad_signature_ratelimiter: Arc<RwLock<HashMap<Vec<String>, RateLimitState>>>,
    pub bad_query_ratelimiter: Arc<RwLock<HashMap<OwnedServerName, RateLimitState>>>,
    pub servername_ratelimiter: Arc<RwLock<HashMap<OwnedServerName, Arc<Semaphore>>>>,
    pub sync_receivers: RwLock<HashMap<(OwnedUserId, OwnedDeviceId), SyncHandle>>,
    pub roomid_mutex_insert: RwLock<HashMap<OwnedRoomId, Arc<Mutex<()>>>>,
    pub roomid_mutex_state: RwLock<HashMap<OwnedRoomId, Arc<Mutex<()>>>>,
    pub roomid_mutex_federation: RwLock<HashMap<OwnedRoomId, Arc<Mutex<()>>>>, // this lock will be held longer
    pub roomid_federationhandletime: RwLock<HashMap<OwnedRoomId, (OwnedEventId, Instant)>>,
    server_user: OwnedUserId,
    admin_alias: OwnedRoomAliasId,
    pub stateres_mutex: Arc<Mutex<()>>,
    pub rotate: RotationHandler,

    pub shutdown: AtomicBool,
}

/// Handles "rotation" of long-polling requests. "Rotation" in this context is similar to "rotation" of log files and the like.
///
/// This is utilized to have sync workers return early and release read locks on the database.
pub struct RotationHandler(broadcast::Sender<()>);

impl RotationHandler {
    pub fn new() -> Self {
        let s = broadcast::channel(1).0;
        Self(s)
    }

    pub fn watch(&self) -> impl Future<Output = ()> {
        let mut r = self.0.subscribe();

        async move {
            let _ = r.recv().await;
        }
    }

    pub fn fire(&self) {
        let _ = self.0.send(());
    }
}

impl Default for RotationHandler {
    fn default() -> Self {
        Self::new()
    }
}

pub struct Resolver {
    inner: GaiResolver,
    overrides: Arc<StdRwLock<TlsNameMap>>,
}

impl Resolver {
    pub fn new(overrides: Arc<StdRwLock<TlsNameMap>>) -> Self {
        Resolver {
            inner: GaiResolver::new(),
            overrides,
        }
    }
}

impl Resolve for Resolver {
    fn resolve(&self, name: Name) -> Resolving {
        self.overrides
            .read()
            .unwrap()
            .get(name.as_str())
            .and_then(|(override_name, port)| {
                override_name.first().map(|first_name| {
                    let x: Box<dyn Iterator<Item = SocketAddr> + Send> =
                        Box::new(iter::once(SocketAddr::new(*first_name, *port)));
                    let x: Resolving = Box::pin(future::ready(Ok(x)));
                    x
                })
            })
            .unwrap_or_else(|| {
                let this = &mut self.inner.clone();
                Box::pin(
                    TowerService::<HyperName>::call(
                        this,
                        // Beautiful hack, please remove this in the future.
                        HyperName::from_str(name.as_str())
                            .expect("reqwest Name is just wrapper for hyper-util Name"),
                    )
                    .map(|result| {
                        result
                            .map(|addrs| -> Addrs { Box::new(addrs) })
                            .map_err(|err| -> Box<dyn StdError + Send + Sync> { Box::new(err) })
                    }),
                )
            })
    }
}

impl Service {
    pub fn load(db: &'static dyn Data, config: Config) -> Result<Self> {
        let keypair = db.load_keypair();

        let keypair = match keypair {
            Ok(k) => k,
            Err(e) => {
                error!("Keypair invalid. Deleting...");
                db.remove_keypair()?;
                return Err(e);
            }
        };

        let tls_name_override = Arc::new(StdRwLock::new(TlsNameMap::new()));

        let jwt_decoding_key = config
            .jwt_secret
            .as_ref()
            .map(|secret| jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()));

        let default_client = reqwest_client_builder(&config)?.build()?;
        let federation_client = reqwest_client_builder(&config)?
            .dns_resolver(Arc::new(Resolver::new(tls_name_override.clone())))
            .build()?;

        // Supported and stable room versions
        let stable_room_versions = vec![
            RoomVersionId::V6,
            RoomVersionId::V7,
            RoomVersionId::V8,
            RoomVersionId::V9,
            RoomVersionId::V10,
            RoomVersionId::V11,
        ];
        // Experimental, partially supported room versions
        let unstable_room_versions = vec![RoomVersionId::V3, RoomVersionId::V4, RoomVersionId::V5];

        let mut s = Self {
            allow_registration: RwLock::new(config.allow_registration),
            admin_alias: RoomAliasId::parse(format!("#admins:{}", &config.server_name))
                .expect("#admins:server_name is a valid alias name"),
            server_user: UserId::parse(format!("@conduit:{}", &config.server_name))
                .expect("@conduit:server_name is valid"),
            db,
            config,
            keypair: Arc::new(keypair),
            dns_resolver: TokioAsyncResolver::tokio_from_system_conf().map_err(|e| {
                error!(
                    "Failed to set up trust dns resolver with system config: {}",
                    e
                );
                Error::bad_config("Failed to set up trust dns resolver with system config.")
            })?,
            actual_destination_cache: Arc::new(RwLock::new(WellKnownMap::new())),
            tls_name_override,
            federation_client,
            default_client,
            jwt_decoding_key,
            stable_room_versions,
            unstable_room_versions,
            bad_event_ratelimiter: Arc::new(RwLock::new(HashMap::new())),
            bad_signature_ratelimiter: Arc::new(RwLock::new(HashMap::new())),
            bad_query_ratelimiter: Arc::new(RwLock::new(HashMap::new())),
            servername_ratelimiter: Arc::new(RwLock::new(HashMap::new())),
            roomid_mutex_state: RwLock::new(HashMap::new()),
            roomid_mutex_insert: RwLock::new(HashMap::new()),
            roomid_mutex_federation: RwLock::new(HashMap::new()),
            roomid_federationhandletime: RwLock::new(HashMap::new()),
            stateres_mutex: Arc::new(Mutex::new(())),
            sync_receivers: RwLock::new(HashMap::new()),
            rotate: RotationHandler::new(),
            shutdown: AtomicBool::new(false),
        };

        // Remove this exception once other media backends are added
        #[allow(irrefutable_let_patterns)]
        if let MediaConfig::FileSystem { path } = &s.config.media {
            fs::create_dir_all(path)?;
        }

        if !s
            .supported_room_versions()
            .contains(&s.config.default_room_version)
        {
            error!(config=?s.config.default_room_version, fallback=?crate::config::default_default_room_version(), "Room version in config isn't supported, falling back to default version");
            s.config.default_room_version = crate::config::default_default_room_version();
        };

        Ok(s)
    }

    /// Returns this server's keypair.
    pub fn keypair(&self) -> &ruma::signatures::Ed25519KeyPair {
        &self.keypair
    }

    /// Returns a reqwest client which can be used to send requests
    pub fn default_client(&self) -> reqwest::Client {
        // Client is cheap to clone (Arc wrapper) and avoids lifetime issues
        self.default_client.clone()
    }

    /// Returns a client used for resolving .well-knowns
    pub fn federation_client(&self) -> reqwest::Client {
        // Client is cheap to clone (Arc wrapper) and avoids lifetime issues
        self.federation_client.clone()
    }

    #[tracing::instrument(skip(self))]
    pub fn next_count(&self) -> Result<u64> {
        self.db.next_count()
    }

    #[tracing::instrument(skip(self))]
    pub fn current_count(&self) -> Result<u64> {
        self.db.current_count()
    }

    #[tracing::instrument(skip(self))]
    pub fn last_check_for_updates_id(&self) -> Result<u64> {
        self.db.last_check_for_updates_id()
    }

    #[tracing::instrument(skip(self))]
    pub fn update_check_for_updates_id(&self, id: u64) -> Result<()> {
        self.db.update_check_for_updates_id(id)
    }

    pub async fn watch(&self, user_id: &UserId, device_id: &DeviceId) -> Result<()> {
        self.db.watch(user_id, device_id).await
    }

    pub fn cleanup(&self) -> Result<()> {
        self.db.cleanup()
    }

    pub fn server_name(&self) -> &ServerName {
        self.config.server_name.as_ref()
    }

    pub fn server_user(&self) -> &UserId {
        self.server_user.as_ref()
    }

    pub fn admin_alias(&self) -> &RoomAliasId {
        self.admin_alias.as_ref()
    }

    pub fn max_request_size(&self) -> u32 {
        self.config.max_request_size
    }

    pub fn max_fetch_prev_events(&self) -> u16 {
        self.config.max_fetch_prev_events
    }

    /// Allows for the temporary (non-persistent) toggling of registration
    pub async fn set_registration(&self, status: bool) {
        let mut lock = self.allow_registration.write().await;
        *lock = status;
    }

    /// Checks whether user registration is allowed
    pub async fn allow_registration(&self) -> bool {
        *self.allow_registration.read().await
    }

    pub fn allow_encryption(&self) -> bool {
        self.config.allow_encryption
    }

    pub fn allow_federation(&self) -> bool {
        self.config.allow_federation
    }

    pub fn allow_room_creation(&self) -> bool {
        self.config.allow_room_creation
    }

    pub fn allow_unstable_room_versions(&self) -> bool {
        self.config.allow_unstable_room_versions
    }

    pub fn default_room_version(&self) -> RoomVersionId {
        self.config.default_room_version.clone()
    }

    pub fn enable_lightning_bolt(&self) -> bool {
        self.config.enable_lightning_bolt
    }

    pub fn allow_check_for_updates(&self) -> bool {
        self.config.allow_check_for_updates
    }

    pub fn trusted_servers(&self) -> &[OwnedServerName] {
        &self.config.trusted_servers
    }

    pub fn turn(&self) -> Option<TurnConfig> {
        // We have to clone basically the entire thing on `/turnServers` otherwise
        self.config.turn.clone()
    }

    pub fn well_known_server(&self) -> OwnedServerName {
        // Same as above, but for /.well-known/matrix/server
        self.config.well_known.server.clone()
    }

    pub fn well_known_client(&self) -> String {
        // Same as above, but for /.well-known/matrix/client
        self.config.well_known.client.clone()
    }

    pub fn dns_resolver(&self) -> &TokioAsyncResolver {
        &self.dns_resolver
    }

    pub fn jwt_decoding_key(&self) -> Option<&jsonwebtoken::DecodingKey> {
        self.jwt_decoding_key.as_ref()
    }

    pub fn emergency_password(&self) -> &Option<String> {
        &self.config.emergency_password
    }

    pub fn supported_room_versions(&self) -> Vec<RoomVersionId> {
        let mut room_versions: Vec<RoomVersionId> = vec![];
        room_versions.extend(self.stable_room_versions.clone());
        if self.allow_unstable_room_versions() {
            room_versions.extend(self.unstable_room_versions.clone());
        };
        room_versions
    }

    /// This doesn't actually check that the keys provided are newer than the old set.
    pub fn add_signing_key_from_trusted_server(
        &self,
        origin: &ServerName,
        new_keys: ServerSigningKeys,
    ) -> Result<SigningKeys> {
        self.db
            .add_signing_key_from_trusted_server(origin, new_keys)
    }

    /// Same as from_trusted_server, except it will move active keys not present in `new_keys` to old_signing_keys
    pub fn add_signing_key_from_origin(
        &self,
        origin: &ServerName,
        new_keys: ServerSigningKeys,
    ) -> Result<SigningKeys> {
        self.db.add_signing_key_from_origin(origin, new_keys)
    }

    /// This returns Ok(None) when there are no keys found for the server.
    pub fn signing_keys_for(&self, origin: &ServerName) -> Result<Option<SigningKeys>> {
        Ok(self.db.signing_keys_for(origin)?.or_else(|| {
            if origin == self.server_name() {
                Some(SigningKeys::load_own_keys())
            } else {
                None
            }
        }))
    }

    /// Filters the key map of multiple servers down to keys that should be accepted given the expiry time,
    /// room version, and timestamp of the parameters
    pub fn filter_keys_server_map(
        &self,
        keys: BTreeMap<String, SigningKeys>,
        timestamp: MilliSecondsSinceUnixEpoch,
        room_version_id: &RoomVersionId,
    ) -> BTreeMap<String, BTreeMap<String, Base64>> {
        keys.into_iter()
            .filter_map(|(server, keys)| {
                self.filter_keys_single_server(keys, timestamp, room_version_id)
                    .map(|keys| (server, keys))
            })
            .collect()
    }

    /// Filters the keys of a single server down to keys that should be accepted given the expiry time,
    /// room version, and timestamp of the parameters
    pub fn filter_keys_single_server(
        &self,
        keys: SigningKeys,
        timestamp: MilliSecondsSinceUnixEpoch,
        room_version_id: &RoomVersionId,
    ) -> Option<BTreeMap<String, Base64>> {
        if keys.valid_until_ts > timestamp
            // valid_until_ts MUST be ignored in room versions 1, 2, 3, and 4.
            // https://spec.matrix.org/v1.10/server-server-api/#get_matrixkeyv2server
                || matches!(room_version_id, RoomVersionId::V1
                    | RoomVersionId::V2
                    | RoomVersionId::V4
                    | RoomVersionId::V3)
        {
            // Given that either the room version allows stale keys, or the valid_until_ts is
            // in the future, all verify_keys are valid
            let mut map: BTreeMap<_, _> = keys
                .verify_keys
                .into_iter()
                .map(|(id, key)| (id, key.key))
                .collect();

            map.extend(keys.old_verify_keys.into_iter().filter_map(|(id, key)| {
                // Even on old room versions, we don't allow old keys if they are expired
                if key.expired_ts > timestamp {
                    Some((id, key.key))
                } else {
                    None
                }
            }));

            Some(map)
        } else {
            None
        }
    }

    pub fn database_version(&self) -> Result<u64> {
        self.db.database_version()
    }

    pub fn bump_database_version(&self, new_version: u64) -> Result<()> {
        self.db.bump_database_version(new_version)
    }

    pub fn get_media_path(&self, media_directory: &str, sha256_hex: &str) -> PathBuf {
        let mut r = PathBuf::new();
        r.push(media_directory);

        //TODO: Directory distribution
        r.push(sha256_hex);

        r
    }

    pub fn shutdown(&self) {
        self.shutdown.store(true, atomic::Ordering::Relaxed);
        // On shutdown
        info!(target: "shutdown-sync", "Received shutdown notification, notifying sync helpers...");
        services().globals.rotate.fire();
    }
}

fn reqwest_client_builder(config: &Config) -> Result<reqwest::ClientBuilder> {
    let mut reqwest_client_builder = reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60 * 3));

    if let Some(proxy) = config.proxy.to_proxy()? {
        reqwest_client_builder = reqwest_client_builder.proxy(proxy);
    }

    Ok(reqwest_client_builder)
}
