pub mod error;

use argon2::{Config, Variant};
use cmp::Ordering;
use rand::prelude::*;
use ring::digest;
use ruma::{
    api::{client::error::ErrorKind, federation::membership::RawStrippedState},
    canonical_json::try_from_json_map,
    events::AnyStrippedStateEvent,
    room_version_rules::RoomVersionRules,
    serde::Raw,
    signatures::Verified,
    CanonicalJsonError, CanonicalJsonObject, CanonicalJsonValue, RoomId,
};
use serde_json::value::to_raw_value;
use std::{
    cmp,
    collections::BTreeMap,
    fmt,
    str::FromStr,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;
use tracing::warn;

use crate::{service::pdu::gen_event_id_canonical_json, services, Error, Result};

pub fn millis_since_unix_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time is valid")
        .as_millis() as u64
}

pub fn secs_since_unix_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time is valid")
        .as_secs()
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

pub fn generate_keypair() -> Vec<u8> {
    let mut value = random_string(8).as_bytes().to_vec();
    value.push(0xff);
    value.extend_from_slice(
        &ruma::signatures::Ed25519KeyPair::generate()
            .expect("Ed25519KeyPair generation always works (?)"),
    );
    value
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
    rand::rng()
        .sample_iter(&rand::distr::Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

/// Calculate a new hash for the given password
pub fn calculate_password_hash(password: &str) -> Result<String, argon2::Error> {
    let hashing_config = Config {
        variant: Variant::Argon2id,
        ..Default::default()
    };

    let salt = random_string(32);
    argon2::hash_encoded(password.as_bytes(), salt.as_bytes(), &hashing_config)
}

#[tracing::instrument(skip(keys))]
pub fn calculate_hash(keys: &[&[u8]]) -> Vec<u8> {
    // We only hash the pdu's event ids, not the whole pdu
    let bytes = keys.join(&0xff);
    let hash = digest::digest(&digest::SHA256, &bytes);
    hash.as_ref().to_owned()
}

pub fn common_elements(
    mut iterators: impl Iterator<Item = impl Iterator<Item = Vec<u8>>>,
    check_order: impl Fn(&[u8], &[u8]) -> Ordering,
) -> Option<impl Iterator<Item = Vec<u8>>> {
    let first_iterator = iterators.next()?;
    let mut other_iterators = iterators.map(|i| i.peekable()).collect::<Vec<_>>();

    Some(first_iterator.filter(move |target| {
        other_iterators.iter_mut().all(|it| {
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

pub fn deserialize_from_str<
    'de,
    D: serde::de::Deserializer<'de>,
    T: FromStr<Err = E>,
    E: fmt::Display,
>(
    deserializer: D,
) -> Result<T, D::Error> {
    struct Visitor<T: FromStr<Err = E>, E>(std::marker::PhantomData<T>);
    impl<T: FromStr<Err = Err>, Err: fmt::Display> serde::de::Visitor<'_> for Visitor<T, Err> {
        type Value = T;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(formatter, "a parsable string")
        }
        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: serde::de::Error,
        {
            v.parse().map_err(serde::de::Error::custom)
        }
    }
    deserializer.deserialize_str(Visitor(std::marker::PhantomData))
}

// Copied from librustdoc:
// https://github.com/rust-lang/rust/blob/cbaeec14f90b59a91a6b0f17fc046c66fa811892/src/librustdoc/html/escape.rs

/// Wrapper struct which will emit the HTML-escaped version of the contained
/// string when passed to a format string.
pub struct HtmlEscape<'a>(pub &'a str);

impl fmt::Display for HtmlEscape<'_> {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Because the internet is always right, turns out there's not that many
        // characters to escape: http://stackoverflow.com/questions/7381974
        let HtmlEscape(s) = *self;
        let pile_o_bits = s;
        let mut last = 0;
        for (i, ch) in s.char_indices() {
            let s = match ch {
                '>' => "&gt;",
                '<' => "&lt;",
                '&' => "&amp;",
                '\'' => "&#39;",
                '"' => "&quot;",
                _ => continue,
            };
            fmt.write_str(&pile_o_bits[last..i])?;
            fmt.write_str(s)?;
            // NOTE: we only expect single byte characters here - which is fine as long as we
            // only match single byte characters
            last = i + 1;
        }

        if last < s.len() {
            fmt.write_str(&pile_o_bits[last..])?;
        }
        Ok(())
    }
}

/// Converts `RawStrippedState` (federation format) into `Raw<AnyStrippedState>` (client format)
pub fn convert_stripped_state(
    stripped_state: Vec<RawStrippedState>,
) -> Result<Vec<Raw<AnyStrippedStateEvent>>> {
    stripped_state
        .into_iter()
        .map(|stripped_state| match stripped_state {
            RawStrippedState::Stripped(state) => Ok(state),
            RawStrippedState::Pdu(state) => {
                let mut event: CanonicalJsonObject =
                    serde_json::from_str(state.get()).map_err(|e| {
                        warn!("Error parsing incoming event {:?}: {:?}", state, e);
                        Error::BadServerResponse("Invalid PDU in server response")
                    })?;

                event.retain(|k, _| {
                    matches!(k.as_str(), "content" | "sender" | "state_key" | "type")
                });

                let raw_value = to_raw_value(&CanonicalJsonValue::Object(event))
                    .expect("To raw json should not fail since only change was adding signature");

                Ok(Raw::<AnyStrippedStateEvent>::from_json(raw_value))
            }
        })
        .collect()
}

/// Performs checks on incoming stripped state, as per [MSC4311]
///
/// [MSC4311]: https://github.com/matrix-org/matrix-spec-proposals/pull/4311
pub async fn check_stripped_state(
    stripped_state: &[RawStrippedState],
    room_id: &RoomId,
    rules: &RoomVersionRules,
) -> Result<()> {
    // Nothing needs to be done for legacy room ids
    if room_id.server_name().is_some() && !rules.authorization.room_create_event_id_as_room_id {
        return Ok(());
    }

    #[cfg(feature = "enforce_msc4311")]
    let mut seen_create_event = false;
    #[cfg(feature = "enforce_msc4311")]
    if !stripped_state.iter().all(|state| match state {
        RawStrippedState::Pdu(_) => true,
        RawStrippedState::Stripped(_) => false,
    }) {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Non-pdu found in stripped state",
        ));
    }

    let stripped_state = stripped_state
        .iter()
        .filter_map(|event| {
            if let RawStrippedState::Pdu(pdu) = event {
                Some(pdu)
            } else {
                None
            }
        })
        .map(|pdu| gen_event_id_canonical_json(pdu, rules))
        .collect::<Result<Vec<_>>>()?;

    let pub_key_map = RwLock::new(BTreeMap::new());

    for (_, pdu) in &stripped_state {
        services()
            .rooms
            .event_handler
            .fetch_required_signing_keys(pdu, &pub_key_map)
            .await?;
    }

    for (event_id, pdu) in stripped_state {
        let filtered_keys = services()
            .rooms
            .event_handler
            .filter_required_signing_keys(&pdu, &pub_key_map, rules)
            .await?;

        if !ruma::signatures::verify_event(&filtered_keys, &pdu, rules)
            .is_ok_and(|verified| verified == Verified::All)
        {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Signature check on stripped state failed",
            ));
        }

        let Some(event_type) = pdu.get("type").and_then(|t| t.as_str()) else {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event with no type returned",
            ));
        };

        if !(event_type == "m.room.create" && rules.authorization.room_create_event_id_as_room_id) {
            let pdu_room_id = pdu
                .get("room_id")
                .ok_or_else(|| Error::BadRequest(ErrorKind::InvalidParam, "Event missing room ID"))
                .map(|v| v.as_str())?
                .ok_or_else(|| {
                    Error::BadRequest(ErrorKind::InvalidParam, "Event has non-string room id")
                })
                .map(RoomId::parse)?
                .map_err(|_| {
                    Error::BadRequest(ErrorKind::InvalidParam, "Event has invalid room ID")
                })?;

            if pdu_room_id != room_id {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Stripped state room ID does not match the one of the request",
                ));
            }
        }

        if event_type == "m.room.create" {
            #[allow(clippy::collapsible_if)]
            if event_id.localpart() != room_id.strip_sigil()
                && rules.authorization.room_create_event_id_as_room_id
            {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Room ID generated from create event does not match that from the request",
                ));
            }

            #[cfg(feature = "enforce_msc4311")]
            {
                seen_create_event = true;
            }
        }
    }

    #[cfg(feature = "enforce_msc4311")]
    if !seen_create_event {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Stripped state contained no valid create PDUs",
        ));
    }

    Ok(())
}
