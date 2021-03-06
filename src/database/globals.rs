use crate::{database::Config, utils, Error, Result};
use log::{error, info};
use ruma::{
    api::federation::discovery::{ServerSigningKeys, VerifyKey},
    EventId, MilliSecondsSinceUnixEpoch, ServerName, ServerSigningKeyId,
};
use rustls::{ServerCertVerifier, WebPKIVerifier};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tokio::sync::Semaphore;
use trust_dns_resolver::TokioAsyncResolver;

pub const COUNTER: &str = "c";

type WellKnownMap = HashMap<Box<ServerName>, (String, String)>;
type TlsNameMap = HashMap<String, webpki::DNSName>;
type RateLimitState = (Instant, u32); // Time if last failed try, number of failed tries
#[derive(Clone)]
pub struct Globals {
    pub actual_destination_cache: Arc<RwLock<WellKnownMap>>, // actual_destination, host
    pub tls_name_override: Arc<RwLock<TlsNameMap>>,
    pub(super) globals: sled::Tree,
    config: Config,
    keypair: Arc<ruma::signatures::Ed25519KeyPair>,
    reqwest_client: reqwest::Client,
    dns_resolver: TokioAsyncResolver,
    jwt_decoding_key: Option<jsonwebtoken::DecodingKey<'static>>,
    pub(super) server_signingkeys: sled::Tree,
    pub bad_event_ratelimiter: Arc<RwLock<BTreeMap<EventId, RateLimitState>>>,
    pub bad_signature_ratelimiter: Arc<RwLock<BTreeMap<Vec<String>, RateLimitState>>>,
    pub servername_ratelimiter: Arc<RwLock<BTreeMap<Box<ServerName>, Arc<Semaphore>>>>,
}

struct MatrixServerVerifier {
    inner: WebPKIVerifier,
    tls_name_override: Arc<RwLock<TlsNameMap>>,
}

impl ServerCertVerifier for MatrixServerVerifier {
    fn verify_server_cert(
        &self,
        roots: &rustls::RootCertStore,
        presented_certs: &[rustls::Certificate],
        dns_name: webpki::DNSNameRef<'_>,
        ocsp_response: &[u8],
    ) -> std::result::Result<rustls::ServerCertVerified, rustls::TLSError> {
        if let Some(override_name) = self.tls_name_override.read().unwrap().get(dns_name.into()) {
            let result = self.inner.verify_server_cert(
                roots,
                presented_certs,
                override_name.as_ref(),
                ocsp_response,
            );
            if result.is_ok() {
                return result;
            }
            info!(
                "Server {:?} is non-compliant, retrying TLS verification with original name",
                dns_name
            );
        }
        self.inner
            .verify_server_cert(roots, presented_certs, dns_name, ocsp_response)
    }
}

impl Globals {
    pub fn load(
        globals: sled::Tree,
        server_signingkeys: sled::Tree,
        config: Config,
    ) -> Result<Self> {
        let bytes = &*globals
            .update_and_fetch("keypair", utils::generate_keypair)?
            .expect("utils::generate_keypair always returns Some");

        let mut parts = bytes.splitn(2, |&b| b == 0xff);

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
            ruma::signatures::Ed25519KeyPair::new(&key, version)
                .map_err(|_| Error::bad_database("Private or public keys are invalid."))
        });

        let keypair = match keypair {
            Ok(k) => k,
            Err(e) => {
                error!("Keypair invalid. Deleting...");
                globals.remove("keypair")?;
                return Err(e);
            }
        };

        let tls_name_override = Arc::new(RwLock::new(TlsNameMap::new()));
        let verifier = Arc::new(MatrixServerVerifier {
            inner: WebPKIVerifier::new(),
            tls_name_override: tls_name_override.clone(),
        });
        let mut tlsconfig = rustls::ClientConfig::new();
        tlsconfig.dangerous().set_certificate_verifier(verifier);
        tlsconfig.root_store =
            rustls_native_certs::load_native_certs().expect("Error loading system certificates");

        let reqwest_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60 * 3))
            .pool_max_idle_per_host(1)
            .use_preconfigured_tls(tlsconfig)
            .build()
            .unwrap();

        let jwt_decoding_key = config
            .jwt_secret
            .as_ref()
            .map(|secret| jsonwebtoken::DecodingKey::from_secret(secret.as_bytes()).into_static());

        Ok(Self {
            globals,
            config,
            keypair: Arc::new(keypair),
            reqwest_client,
            dns_resolver: TokioAsyncResolver::tokio_from_system_conf().map_err(|_| {
                Error::bad_config("Failed to set up trust dns resolver with system config.")
            })?,
            actual_destination_cache: Arc::new(RwLock::new(WellKnownMap::new())),
            tls_name_override,
            server_signingkeys,
            jwt_decoding_key,
            bad_event_ratelimiter: Arc::new(RwLock::new(BTreeMap::new())),
            bad_signature_ratelimiter: Arc::new(RwLock::new(BTreeMap::new())),
            servername_ratelimiter: Arc::new(RwLock::new(BTreeMap::new())),
        })
    }

    /// Returns this server's keypair.
    pub fn keypair(&self) -> &ruma::signatures::Ed25519KeyPair {
        &self.keypair
    }

    /// Returns a reqwest client which can be used to send requests.
    pub fn reqwest_client(&self) -> &reqwest::Client {
        &self.reqwest_client
    }

    pub fn next_count(&self) -> Result<u64> {
        Ok(utils::u64_from_bytes(
            &self
                .globals
                .update_and_fetch(COUNTER, utils::increment)?
                .expect("utils::increment will always put in a value"),
        )
        .map_err(|_| Error::bad_database("Count has invalid bytes."))?)
    }

    pub fn current_count(&self) -> Result<u64> {
        self.globals.get(COUNTER)?.map_or(Ok(0_u64), |bytes| {
            Ok(utils::u64_from_bytes(&bytes)
                .map_err(|_| Error::bad_database("Count has invalid bytes."))?)
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

    pub fn trusted_servers(&self) -> &[Box<ServerName>] {
        &self.config.trusted_servers
    }

    pub fn dns_resolver(&self) -> &TokioAsyncResolver {
        &self.dns_resolver
    }

    pub fn jwt_decoding_key(&self) -> Option<&jsonwebtoken::DecodingKey<'_>> {
        self.jwt_decoding_key.as_ref()
    }

    /// TODO: the key valid until timestamp is only honored in room version > 4
    /// Remove the outdated keys and insert the new ones.
    ///
    /// This doesn't actually check that the keys provided are newer than the old set.
    pub fn add_signing_key(&self, origin: &ServerName, new_keys: &ServerSigningKeys) -> Result<()> {
        self.server_signingkeys
            .update_and_fetch(origin.as_bytes(), |signingkeys| {
                let mut keys = signingkeys
                    .and_then(|keys| serde_json::from_slice(keys).ok())
                    .unwrap_or_else(|| {
                        // Just insert "now", it doesn't matter
                        ServerSigningKeys::new(origin.to_owned(), MilliSecondsSinceUnixEpoch::now())
                    });
                keys.verify_keys
                    .extend(new_keys.verify_keys.clone().into_iter());
                keys.old_verify_keys
                    .extend(new_keys.old_verify_keys.clone().into_iter());
                Some(serde_json::to_vec(&keys).expect("serversigningkeys can be serialized"))
            })?;

        Ok(())
    }

    /// This returns an empty `Ok(BTreeMap<..>)` when there are no keys found for the server.
    pub fn signing_keys_for(
        &self,
        origin: &ServerName,
    ) -> Result<BTreeMap<ServerSigningKeyId, VerifyKey>> {
        let signingkeys = self
            .server_signingkeys
            .get(origin.as_bytes())?
            .and_then(|bytes| serde_json::from_slice::<ServerSigningKeys>(&bytes).ok())
            .map(|keys| {
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
        self.globals.get("version")?.map_or(Ok(0), |version| {
            utils::u64_from_bytes(&version)
                .map_err(|_| Error::bad_database("Database version id is invalid."))
        })
    }

    pub fn bump_database_version(&self, new_version: u64) -> Result<()> {
        self.globals.insert("version", &new_version.to_be_bytes())?;
        Ok(())
    }
}
