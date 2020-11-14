use crate::{utils, Error, Result};
use log::error;
use ruma::ServerName;
use std::{convert::TryInto, sync::Arc};

pub const COUNTER: &str = "c";

#[derive(Clone)]
pub struct Globals {
    pub(super) globals: sled::Tree,
    keypair: Arc<ruma::signatures::Ed25519KeyPair>,
    reqwest_client: reqwest::Client,
    server_name: Box<ServerName>,
    max_request_size: u32,
    registration_disabled: bool,
    encryption_disabled: bool,
    federation_enabled: bool,
}

impl Globals {
    pub fn load(globals: sled::Tree, config: &rocket::Config) -> Result<Self> {
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

        Ok(Self {
            globals,
            keypair: Arc::new(keypair),
            reqwest_client: reqwest::Client::new(),
            server_name: config
                .get_str("server_name")
                .unwrap_or("localhost")
                .to_string()
                .try_into()
                .map_err(|_| Error::bad_config("Invalid server_name."))?,
            max_request_size: config
                .get_int("max_request_size")
                .unwrap_or(20 * 1024 * 1024) // Default to 20 MB
                .try_into()
                .map_err(|_| Error::bad_config("Invalid max_request_size."))?,
            registration_disabled: config.get_bool("registration_disabled").unwrap_or(false),
            encryption_disabled: config.get_bool("encryption_disabled").unwrap_or(false),
            federation_enabled: config.get_bool("federation_enabled").unwrap_or(false),
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
        self.server_name.as_ref()
    }

    pub fn max_request_size(&self) -> u32 {
        self.max_request_size
    }

    pub fn registration_disabled(&self) -> bool {
        self.registration_disabled
    }

    pub fn encryption_disabled(&self) -> bool {
        self.encryption_disabled
    }

    pub fn federation_enabled(&self) -> bool {
        self.federation_enabled
    }
}
