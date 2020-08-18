use crate::Error;
use argon2::{Config, Variant};
use rand::prelude::*;
use std::{
    convert::TryInto,
    time::{SystemTime, UNIX_EPOCH},
};

pub fn millis_since_unix_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time is valid")
        .as_millis() as u64
}

pub fn increment(old: Option<&[u8]>) -> Option<Vec<u8>> {
    let number = match old.map(|bytes| bytes.try_into()) {
        Some(Ok(bytes)) => {
            let number = u64::from_be_bytes(bytes);
            number + 1
        }
        _ => 1, // Start at one. since 0 should return the first event in the db
    };

    Some(number.to_be_bytes().to_vec())
}

pub fn generate_keypair(old: Option<&[u8]>) -> Option<Vec<u8>> {
    Some(old.map(|s| s.to_vec()).unwrap_or_else(|| {
        ruma::signatures::Ed25519KeyPair::generate()
            .expect("Ed25519KeyPair generation always works (?)")
    }))
}

/// Parses the bytes into an u64.
pub fn u64_from_bytes(bytes: &[u8]) -> Result<u64, std::array::TryFromSliceError> {
    let array: [u8; 8] = bytes.try_into()?;
    Ok(u64::from_be_bytes(array))
}

/// Parses the bytes into a string.
pub fn string_from_bytes(bytes: &[u8]) -> Result<String, std::string::FromUtf8Error> {
    String::from_utf8(bytes.to_vec())
}

pub fn random_string(length: usize) -> String {
    thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(length)
        .collect()
}

/// Calculate a new hash for the given password
pub fn calculate_hash(password: &str) -> Result<String, argon2::Error> {
    let hashing_config = Config {
        variant: Variant::Argon2id,
        ..Default::default()
    };

    let salt = random_string(32);
    argon2::hash_encoded(password.as_bytes(), salt.as_bytes(), &hashing_config)
}

pub fn deserialize<'de, T: serde::Deserialize<'de>>(val: &'de sled::IVec) -> Result<T, Error> {
    serde_json::from_slice::<T>(val.as_ref())
        .map_err(|_| Error::bad_database("Found invalid bytes as PDU in db."))
}
