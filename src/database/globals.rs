use crate::{database::Config, utils, Error, Result};
use log::error;
use ruma::ServerName;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;
use trust_dns_resolver::TokioAsyncResolver;

pub const COUNTER: &str = "c";

#[derive(Clone)]
pub struct Globals {
    pub actual_destination_cache: Arc<RwLock<HashMap<Box<ServerName>, (String, Option<String>)>>>, // actual_destination, host
    pub(super) globals: sled::Tree,
    config: Config,
    keypair: Arc<ruma::signatures::Ed25519KeyPair>,
    reqwest_client: reqwest::Client,
    dns_resolver: TokioAsyncResolver,
    jwt_decoding_key: Option<jsonwebtoken::DecodingKey<'static>>,
}

impl Globals {
    pub async fn load(globals: sled::Tree, config: Config) -> Result<Self> {
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

        let reqwest_client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .timeout(Duration::from_secs(60 * 3))
            .pool_max_idle_per_host(1)
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
            actual_destination_cache: Arc::new(RwLock::new(HashMap::new())),
            jwt_decoding_key,
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

    pub fn dns_resolver(&self) -> &TokioAsyncResolver {
        &self.dns_resolver
    }

    pub fn jwt_decoding_key(&self) -> Option<&jsonwebtoken::DecodingKey<'_>> {
        self.jwt_decoding_key.as_ref()
    }
}
