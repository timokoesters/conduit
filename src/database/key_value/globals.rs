use ruma::signatures::Ed25519KeyPair;

use crate::{Result, service, database::KeyValueDatabase, Error, utils};

impl service::globals::Data for KeyValueDatabase {
    fn load_keypair(&self) -> Result<Ed25519KeyPair> {
        let keypair_bytes = self.globals.get(b"keypair")?.map_or_else(
            || {
                let keypair = utils::generate_keypair();
                self.globals.insert(b"keypair", &keypair)?;
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
            Ed25519KeyPair::from_der(key, version)
                .map_err(|_| Error::bad_database("Private or public keys are invalid."))
        });
    }
    fn remove_keypair(&self) -> Result<()> {
        self.globals.remove(b"keypair")?
    }
}
