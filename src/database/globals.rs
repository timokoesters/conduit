use super::appservices;
use crate::{utils, Error, Result};

pub const COUNTER: &str = "c";

pub struct Globals {
    pub(super) globals: sled::Tree,
    keypair: ruma::signatures::Ed25519KeyPair,
    reqwest_client: reqwest::Client,
    appservices: appservices::AppServices,
    server_name: String,
    registration_disabled: bool,
}

impl Globals {
    pub fn load(globals: sled::Tree, config: &rocket::Config) -> Result<Self> {
        let server_name = config
            .get_str("server_name")
            .unwrap_or("localhost")
            .to_owned();

        let keypair = ruma::signatures::Ed25519KeyPair::new(
            &*globals
                .update_and_fetch("keypair", utils::generate_keypair)?
                .expect("utils::generate_keypair always returns Some"),
            "key1".to_owned(),
        )
        .map_err(|_| Error::bad_database("Private or public keys are invalid."))?;

        let appservices = appservices::AppServices::new(
            config
                .get_slice("appservice_registrations")
                .unwrap_or(&vec![])
                .iter()
                .map(|reg| {
                    reg.clone()
                        .try_into::<String>()
                        .expect("appservice registration should be a path to a file")
                }),
            &server_name,
        )?;

        Ok(Self {
            globals,
            keypair,
            reqwest_client: reqwest::Client::new(),
            appservices,
            server_name,
            registration_disabled: config.get_bool("registration_disabled").unwrap_or(false),
        })
    }

    /// Returns the server_name of the server.
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Returns the appservice registrations of the server.
    pub fn appservices(&self) -> &appservices::AppServices {
        &self.appservices
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

    pub fn registration_disabled(&self) -> bool {
        self.registration_disabled
    }
}
