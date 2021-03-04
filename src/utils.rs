use argon2::{Config, Variant};
use cmp::Ordering;
use rand::prelude::*;
use ruma::serde::{try_from_json_map, CanonicalJsonError, CanonicalJsonObject};
use sled::IVec;
use std::{
    cmp,
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
        let mut value = random_string(8).as_bytes().to_vec();
        value.push(0xff);
        value.extend_from_slice(
            &ruma::signatures::Ed25519KeyPair::generate()
                .expect("Ed25519KeyPair generation always works (?)"),
        );
        value
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
        .map(char::from)
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

pub fn common_elements(
    mut iterators: impl Iterator<Item = impl Iterator<Item = IVec>>,
    check_order: impl Fn(&IVec, &IVec) -> Ordering,
) -> Option<impl Iterator<Item = IVec>> {
    let first_iterator = iterators.next()?;
    let mut other_iterators = iterators.map(|i| i.peekable()).collect::<Vec<_>>();

    Some(first_iterator.filter(move |target| {
        other_iterators
            .iter_mut()
            .map(|it| {
                while let Some(element) = it.peek() {
                    match check_order(element, target) {
                        Ordering::Greater => return false, // We went too far
                        Ordering::Equal => return true,    // Element is in both iters
                        Ordering::Less => {
                            // Keep searching
                            it.next();
                        }
                    }
                }
                false
            })
            .all(|b| b)
    }))
}

/// Fallible conversion from any value that implements `Serialize` to a `CanonicalJsonObject`.
///
/// `value` must serialize to an `serde_json::Value::Object`.
pub fn to_canonical_object<T: serde::Serialize>(
    value: T,
) -> Result<CanonicalJsonObject, CanonicalJsonError> {
    use serde::ser::Error;

    match serde_json::to_value(value).map_err(CanonicalJsonError::SerDe)? {
        serde_json::Value::Object(map) => try_from_json_map(map),
        _ => Err(CanonicalJsonError::SerDe(serde_json::Error::custom(
            "Value must be an object",
        ))),
    }
}
