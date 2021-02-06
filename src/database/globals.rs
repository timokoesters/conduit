use crate::{database::Config, utils, Error, Result};
use log::error;
use ruma::{
    api::federation::discovery::{ServerSigningKeys, VerifyKey},
    ServerName, ServerSigningKeyId,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, RwLock},
    time::Duration,
};
use trust_dns_resolver::TokioAsyncResolver;

pub const COUNTER: &str = "c";

pub type DestinationCache = Arc<RwLock<HashMap<Box<ServerName>, (String, Option<String>)>>>;

#[derive(Clone)]
pub struct Globals {
    pub(super) globals: sled::Tree,
    config: Config,
    keypair: Arc<ruma::signatures::Ed25519KeyPair>,
    reqwest_client: reqwest::Client,
    pub actual_destination_cache: DestinationCache, // actual_destination, host
    dns_resolver: TokioAsyncResolver,
    pub(super) servertimeout_signingkey: sled::Tree, // ServerName -> algorithm:key + pubkey
}

impl Globals {
    pub fn load(globals: sled::Tree, server_keys: sled::Tree, config: Config) -> Result<Self> {
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

        Ok(Self {
            globals,
            config,
            keypair: Arc::new(keypair),
            reqwest_client,
            dns_resolver: TokioAsyncResolver::tokio_from_system_conf().map_err(|_| {
                Error::bad_config("Failed to set up trust dns resolver with system config.")
            })?,
            actual_destination_cache: Arc::new(RwLock::new(HashMap::new())),
            servertimeout_signingkey: server_keys,
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

    /// TODO: the key valid until timestamp is only honored in room version > 4
    /// Remove the outdated keys and insert the new ones.
    ///
    /// This doesn't actually check that the keys provided are newer than the old set.
    pub fn add_signing_key(&self, origin: &ServerName, keys: &ServerSigningKeys) -> Result<()> {
        // Remove outdated keys
        let now = crate::utils::millis_since_unix_epoch();
        for item in self.servertimeout_signingkey.scan_prefix(origin.as_bytes()) {
            let (k, _) = item?;
            let valid_until = k
                .splitn(2, |&b| b == 0xff)
                .nth(1)
                .map(crate::utils::u64_from_bytes)
                .ok_or_else(|| Error::bad_database("Invalid signing keys."))?
                .map_err(|_| Error::bad_database("Invalid signing key valid until bytes"))?;

            if now > valid_until {
                self.servertimeout_signingkey.remove(k)?;
            }
        }

        let mut key = origin.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(
            &(keys
                .valid_until_ts
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time is valid")
                .as_millis() as u64)
                .to_be_bytes(),
        );

        self.servertimeout_signingkey.insert(
            key,
            serde_json::to_vec(&keys.verify_keys).expect("ServerSigningKeys are a valid string"),
        )?;
        Ok(())
    }

    /// This returns an empty `Ok(BTreeMap<..>)` when there are no keys found for the server.
    pub fn signing_keys_for(
        &self,
        origin: &ServerName,
    ) -> Result<BTreeMap<ServerSigningKeyId, VerifyKey>> {
        let now = crate::utils::millis_since_unix_epoch();
        for item in self.servertimeout_signingkey.scan_prefix(origin.as_bytes()) {
            let (k, bytes) = item?;
            let valid_until = k
                .splitn(2, |&b| b == 0xff)
                .nth(1)
                .map(crate::utils::u64_from_bytes)
                .ok_or_else(|| Error::bad_database("Invalid signing keys."))?
                .map_err(|_| Error::bad_database("Invalid signing key valid until bytes"))?;
            // If these keys are still valid use em!
            if valid_until > now {
                return serde_json::from_slice(&bytes)
                    .map_err(|_| Error::bad_database("Invalid BTreeMap<> of signing keys"));
            }
        }
        Ok(BTreeMap::default())
    }
}
