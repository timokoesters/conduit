mod data;
pub use data::Data;
use ruma::{
    serde::Base64, OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedServerName,
    OwnedServerSigningKeyId, OwnedUserId,
};

use crate::api::server_server::FedDest;

use crate::{services, Config, Error, Result};
use futures_util::FutureExt;
use hickory_resolver::TokioAsyncResolver;
use hyper::{
    client::connect::dns::{GaiResolver, Name},
    service::Service as HyperService,
};
use reqwest::dns::{Addrs, Resolve, Resolving};
use ruma::{
    api::{
        client::sync::sync_events,
        federation::discovery::{ServerSigningKeys, VerifyKey},
    },
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
    sync::{
        atomic::{self, AtomicBool},
        Arc, RwLock as StdRwLock,
    },
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, watch::Receiver, Mutex, RwLock, Semaphore};
use tracing::{error, info};

use base64::{engine::general_purpose, Engine as _};

type WellKnownMap = HashMap<OwnedServerName, (FedDest, String)>;
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
    pub stateres_mutex: Arc<Mutex<()>>,
    pub rotate: RotationHandler,

    pub shutdown: AtomicBool,
}

/// Handles "rotation" of long-polling requests. "Rotation" in this context is similar to "rotation" of log files and the like.
///
/// This is utilized to have sync workers return early and release read locks on the database.
pub struct RotationHandler(broadcast::Sender<()>, broadcast::Receiver<()>);

impl RotationHandler {
    pub fn new() -> Self {
        let (s, r) = broadcast::channel(1);
        Self(s, r)
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
                Box::pin(HyperService::<Name>::call(this, name).map(|result| {
                    result
                        .map(|addrs| -> Addrs { Box::new(addrs) })
                        .map_err(|err| -> Box<dyn StdError + Send + Sync> { Box::new(err) })
                }))
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

        fs::create_dir_all(s.get_media_folder())?;

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

    pub fn max_request_size(&self) -> u32 {
        self.config.max_request_size
    }

    pub fn max_fetch_prev_events(&self) -> u16 {
        self.config.max_fetch_prev_events
    }

    pub fn allow_registration(&self) -> bool {
        self.config.allow_registration
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

    pub fn dns_resolver(&self) -> &TokioAsyncResolver {
        &self.dns_resolver
    }

    pub fn jwt_decoding_key(&self) -> Option<&jsonwebtoken::DecodingKey> {
        self.jwt_decoding_key.as_ref()
    }

    pub fn turn_password(&self) -> &String {
        &self.config.turn_password
    }

    pub fn turn_ttl(&self) -> u64 {
        self.config.turn_ttl
    }

    pub fn turn_uris(&self) -> &[String] {
        &self.config.turn_uris
    }

    pub fn turn_username(&self) -> &String {
        &self.config.turn_username
    }

    pub fn turn_secret(&self) -> &String {
        &self.config.turn_secret
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

    /// TODO: the key valid until timestamp is only honored in room version > 4
    /// Remove the outdated keys and insert the new ones.
    ///
    /// This doesn't actually check that the keys provided are newer than the old set.
    pub fn add_signing_key(
        &self,
        origin: &ServerName,
        new_keys: ServerSigningKeys,
    ) -> Result<BTreeMap<OwnedServerSigningKeyId, VerifyKey>> {
        self.db.add_signing_key(origin, new_keys)
    }

    /// This returns an empty `Ok(BTreeMap<..>)` when there are no keys found for the server.
    pub fn signing_keys_for(
        &self,
        origin: &ServerName,
    ) -> Result<BTreeMap<OwnedServerSigningKeyId, VerifyKey>> {
        let mut keys = self.db.signing_keys_for(origin)?;
        if origin == self.server_name() {
            keys.insert(
                format!("ed25519:{}", services().globals.keypair().version())
                    .try_into()
                    .expect("found invalid server signing keys in DB"),
                VerifyKey {
                    key: Base64::new(self.keypair.public_key().to_vec()),
                },
            );
        }

        Ok(keys)
    }

    pub fn database_version(&self) -> Result<u64> {
        self.db.database_version()
    }

    pub fn bump_database_version(&self, new_version: u64) -> Result<()> {
        self.db.bump_database_version(new_version)
    }

    pub fn get_media_folder(&self) -> PathBuf {
        let mut r = PathBuf::new();
        r.push(self.config.database_path.clone());
        r.push("media");
        r
    }

    pub fn get_media_file(&self, key: &[u8]) -> PathBuf {
        let mut r = PathBuf::new();
        r.push(self.config.database_path.clone());
        r.push("media");
        r.push(general_purpose::URL_SAFE_NO_PAD.encode(key));
        r
    }

    pub fn well_known_client(&self) -> &Option<String> {
        &self.config.well_known_client
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
