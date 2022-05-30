use crate::{database::Config, server_server::FedDest, utils, Error, Result};
use ruma::{
    api::{
        client::sync::sync_events,
        federation::discovery::{ServerSigningKeys, VerifyKey},
    },
    DeviceId, EventId, MilliSecondsSinceUnixEpoch, RoomId, RoomVersionId, ServerName,
    ServerSigningKeyId, UserId,
};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    future::Future,
    net::{IpAddr, SocketAddr},
    path::PathBuf,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, watch::Receiver, Mutex as TokioMutex, Semaphore};
use tracing::error;
use trust_dns_resolver::TokioAsyncResolver;

use super::abstraction::Tree;

pub const COUNTER: &[u8] = b"c";

type WellKnownMap = HashMap<Box<ServerName>, (FedDest, String)>;
type TlsNameMap = HashMap<String, (Vec<IpAddr>, u16)>;
type RateLimitState = (Instant, u32); // Time if last failed try, number of failed tries
type SyncHandle = (
    Option<String>,                                      // since
    Receiver<Option<Result<sync_events::v3::Response>>>, // rx
);

pub struct Globals {
    pub actual_destination_cache: Arc<RwLock<WellKnownMap>>, // actual_destination, host
    pub tls_name_override: Arc<RwLock<TlsNameMap>>,
    pub(super) globals: Arc<dyn Tree>,
    pub config: Config,
    keypair: Arc<ruma::signatures::Ed25519KeyPair>,
    dns_resolver: TokioAsyncResolver,
    jwt_decoding_key: Option<jsonwebtoken::DecodingKey<'static>>,
    federation_client: reqwest::Client,
    default_client: reqwest::Client,
    pub stable_room_versions: Vec<RoomVersionId>,
    pub unstable_room_versions: Vec<RoomVersionId>,
    pub(super) server_signingkeys: Arc<dyn Tree>,
    pub bad_event_ratelimiter: Arc<RwLock<HashMap<Box<EventId>, RateLimitState>>>,
    pub bad_signature_ratelimiter: Arc<RwLock<HashMap<Vec<String>, RateLimitState>>>,
    pub servername_ratelimiter: Arc<RwLock<HashMap<Box<ServerName>, Arc<Semaphore>>>>,
    pub sync_receivers: RwLock<HashMap<(Box<UserId>, Box<DeviceId>), SyncHandle>>,
    pub roomid_mutex_insert: RwLock<HashMap<Box<RoomId>, Arc<Mutex<()>>>>,
    pub roomid_mutex_state: RwLock<HashMap<Box<RoomId>, Arc<TokioMutex<()>>>>,
    pub roomid_mutex_federation: RwLock<HashMap<Box<RoomId>, Arc<TokioMutex<()>>>>, // this lock will be held longer
    pub roomid_federationhandletime: RwLock<HashMap<Box<RoomId>, (Box<EventId>, Instant)>>,
    pub stateres_mutex: Arc<Mutex<()>>,
    pub rotate: RotationHandler,
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

impl Globals {
    pub fn load(
        globals: Arc<dyn Tree>,
        server_signingkeys: Arc<dyn Tree>,
        config: Config,
    ) -> Result<Self> {
        let keypair_bytes = globals.get(b"keypair")?.map_or_else(
            || {
                let keypair = utils::generate_keypair();
                globals.insert(b"keypair", &keypair)?;
                Ok::<_, Error>(keypair)
            },
            |s| Ok(s.to_vec()),
        )?;

        let mut parts = keypair_bytes.splitn(2, |&b| b == 0xff);

        let keypair = utils::string_from_bytes(
            // 1. version
            parts
                .next()
                .expect("splitn always returns at least one element"),
        )
        .map_err(|_| Error::bad_database("Invalid version bytes in keypair."))
        .and_then(|version| {
            // 2. key
            parts
                .next()
                .ok_or_else(|| Error::bad_database("Invalid keypair format in database."))
                .map(|key| (version, key))
        })
        .and_then(|(version, key)| {
            ruma::signatures::Ed25519KeyPair::from_der(key, version)
                .map_err(|_| Error::bad_database("Private or public keys are invalid."))
        });

        let keypair = match keypair {
            Ok(k) => k,
            Err(e) => {
                error!("Keypair invalid. Deleting...");
                globals.remove(b"keypair")?;
                return Err(e);
            }
        };

        let tls_name_override = Arc::new(RwLock::new(TlsNameMap::new()));

        let jwt_decoding_key = config
            .jwt_secret
            .as_ref()
            .map(|secret| jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()).into_static());

        let default_client = reqwest_client_builder(&config)?.build()?;
        let name_override = Arc::clone(&tls_name_override);
        let federation_client = reqwest_client_builder(&config)?
            .resolve_fn(move |domain| {
                let read_guard = name_override.read().unwrap();
                let (override_name, port) = read_guard.get(&domain)?;
                let first_name = override_name.get(0)?;
                Some(SocketAddr::new(*first_name, *port))
            })
            .build()?;

        // Supported and stable room versions
        let stable_room_versions = vec![
            RoomVersionId::V6,
            RoomVersionId::V7,
            RoomVersionId::V8,
            RoomVersionId::V9,
        ];
        // Experimental, partially supported room versions
        let unstable_room_versions = vec![RoomVersionId::V3, RoomVersionId::V4, RoomVersionId::V5];

        let mut s = Self {
            globals,
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
            server_signingkeys,
            jwt_decoding_key,
            stable_room_versions,
            unstable_room_versions,
            bad_event_ratelimiter: Arc::new(RwLock::new(HashMap::new())),
            bad_signature_ratelimiter: Arc::new(RwLock::new(HashMap::new())),
            servername_ratelimiter: Arc::new(RwLock::new(HashMap::new())),
            roomid_mutex_state: RwLock::new(HashMap::new()),
            roomid_mutex_insert: RwLock::new(HashMap::new()),
            roomid_mutex_federation: RwLock::new(HashMap::new()),
            roomid_federationhandletime: RwLock::new(HashMap::new()),
            stateres_mutex: Arc::new(Mutex::new(())),
            sync_receivers: RwLock::new(HashMap::new()),
            rotate: RotationHandler::new(),
        };

        fs::create_dir_all(s.get_media_folder())?;

        if !s
            .supported_room_versions()
            .contains(&s.config.default_room_version)
        {
            error!("Room version in config isn't supported, falling back to Version 6");
            s.config.default_room_version = RoomVersionId::V6;
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
        utils::u64_from_bytes(&self.globals.increment(COUNTER)?)
            .map_err(|_| Error::bad_database("Count has invalid bytes."))
    }

    #[tracing::instrument(skip(self))]
    pub fn current_count(&self) -> Result<u64> {
        self.globals.get(COUNTER)?.map_or(Ok(0_u64), |bytes| {
            utils::u64_from_bytes(&bytes)
                .map_err(|_| Error::bad_database("Count has invalid bytes."))
        })
    }

    pub fn server_name(&self) -> &ServerName {
        self.config.server_name.as_ref()
    }

    pub fn max_request_size(&self) -> u32 {
        self.config.max_request_size
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

    pub fn trusted_servers(&self) -> &[Box<ServerName>] {
        &self.config.trusted_servers
    }

    pub fn dns_resolver(&self) -> &TokioAsyncResolver {
        &self.dns_resolver
    }

    pub fn jwt_decoding_key(&self) -> Option<&jsonwebtoken::DecodingKey<'_>> {
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
    ) -> Result<BTreeMap<Box<ServerSigningKeyId>, VerifyKey>> {
        // Not atomic, but this is not critical
        let signingkeys = self.server_signingkeys.get(origin.as_bytes())?;

        let mut keys = signingkeys
            .and_then(|keys| serde_json::from_slice(&keys).ok())
            .unwrap_or_else(|| {
                // Just insert "now", it doesn't matter
                ServerSigningKeys::new(origin.to_owned(), MilliSecondsSinceUnixEpoch::now())
            });

        let ServerSigningKeys {
            verify_keys,
            old_verify_keys,
            ..
        } = new_keys;

        keys.verify_keys.extend(verify_keys.into_iter());
        keys.old_verify_keys.extend(old_verify_keys.into_iter());

        self.server_signingkeys.insert(
            origin.as_bytes(),
            &serde_json::to_vec(&keys).expect("serversigningkeys can be serialized"),
        )?;

        let mut tree = keys.verify_keys;
        tree.extend(
            keys.old_verify_keys
                .into_iter()
                .map(|old| (old.0, VerifyKey::new(old.1.key))),
        );

        Ok(tree)
    }

    /// This returns an empty `Ok(BTreeMap<..>)` when there are no keys found for the server.
    pub fn signing_keys_for(
        &self,
        origin: &ServerName,
    ) -> Result<BTreeMap<Box<ServerSigningKeyId>, VerifyKey>> {
        let signingkeys = self
            .server_signingkeys
            .get(origin.as_bytes())?
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .map(|keys: ServerSigningKeys| {
                let mut tree = keys.verify_keys;
                tree.extend(
                    keys.old_verify_keys
                        .into_iter()
                        .map(|old| (old.0, VerifyKey::new(old.1.key))),
                );
                tree
            })
            .unwrap_or_else(BTreeMap::new);

        Ok(signingkeys)
    }

    pub fn database_version(&self) -> Result<u64> {
        self.globals.get(b"version")?.map_or(Ok(0), |version| {
            utils::u64_from_bytes(&version)
                .map_err(|_| Error::bad_database("Database version id is invalid."))
        })
    }

    pub fn bump_database_version(&self, new_version: u64) -> Result<()> {
        self.globals
            .insert(b"version", &new_version.to_be_bytes())?;
        Ok(())
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
        r.push(base64::encode_config(key, base64::URL_SAFE_NO_PAD));
        r
    }
}

fn reqwest_client_builder(config: &Config) -> Result<reqwest::ClientBuilder> {
    let mut reqwest_client_builder = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .timeout(Duration::from_secs(60 * 3));

    if let Some(proxy) = config.proxy.to_proxy()? {
        reqwest_client_builder = reqwest_client_builder.proxy(proxy);
    }

    Ok(reqwest_client_builder)
}
