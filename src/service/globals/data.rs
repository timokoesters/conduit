use std::{
    collections::BTreeMap,
    time::{Duration, SystemTime},
};

use crate::{services, Result};
use async_trait::async_trait;
use ruma::{
    api::federation::discovery::{OldVerifyKey, ServerSigningKeys, VerifyKey},
    serde::Base64,
    signatures::Ed25519KeyPair,
    DeviceId, MilliSecondsSinceUnixEpoch, ServerName, UserId,
};
use serde::Deserialize;

/// Similar to ServerSigningKeys, but drops a few unnecessary fields we don't require post-validation
#[derive(Deserialize, Debug, Clone)]
pub struct SigningKeys {
    pub verify_keys: BTreeMap<String, VerifyKey>,
    pub old_verify_keys: BTreeMap<String, OldVerifyKey>,
    pub valid_until_ts: MilliSecondsSinceUnixEpoch,
}

impl SigningKeys {
    /// Creates the SigningKeys struct, using the keys of the current server
    pub fn load_own_keys() -> Self {
        let mut keys = Self {
            verify_keys: BTreeMap::new(),
            old_verify_keys: BTreeMap::new(),
            valid_until_ts: MilliSecondsSinceUnixEpoch::from_system_time(
                SystemTime::now() + Duration::from_secs(7 * 86400),
            )
            .expect("Should be valid until year 500,000,000"),
        };

        keys.verify_keys.insert(
            format!("ed25519:{}", services().globals.keypair().version()),
            VerifyKey {
                key: Base64::new(services().globals.keypair.public_key().to_vec()),
            },
        );

        keys
    }
}

impl From<ServerSigningKeys> for SigningKeys {
    fn from(value: ServerSigningKeys) -> Self {
        let ServerSigningKeys {
            verify_keys,
            old_verify_keys,
            valid_until_ts,
            ..
        } = value;

        Self {
            verify_keys: verify_keys
                .into_iter()
                .map(|(id, key)| (id.to_string(), key))
                .collect(),
            old_verify_keys: old_verify_keys
                .into_iter()
                .map(|(id, key)| (id.to_string(), key))
                .collect(),
            valid_until_ts,
        }
    }
}

#[async_trait]
pub trait Data: Send + Sync {
    fn next_count(&self) -> Result<u64>;
    fn current_count(&self) -> Result<u64>;
    fn last_check_for_updates_id(&self) -> Result<u64>;
    fn update_check_for_updates_id(&self, id: u64) -> Result<()>;
    async fn watch(&self, user_id: &UserId, device_id: &DeviceId) -> Result<()>;
    fn cleanup(&self) -> Result<()>;
    fn memory_usage(&self) -> String;
    fn clear_caches(&self, amount: u32);
    fn load_keypair(&self) -> Result<Ed25519KeyPair>;
    fn remove_keypair(&self) -> Result<()>;
    /// Only extends the cached keys, not moving any verify_keys to old_verify_keys, as if we suddenly
    /// recieve requests from the origin server, we want to be able to accept requests from them
    fn add_signing_key_from_trusted_server(
        &self,
        origin: &ServerName,
        new_keys: ServerSigningKeys,
    ) -> Result<SigningKeys>;
    /// Extends cached keys, as well as moving verify_keys that are not present in these new keys to
    /// old_verify_keys, so that potnetially comprimised keys cannot be used to make requests
    fn add_signing_key_from_origin(
        &self,
        origin: &ServerName,
        new_keys: ServerSigningKeys,
    ) -> Result<SigningKeys>;

    /// This returns an empty `Ok(BTreeMap<..>)` when there are no keys found for the server.
    fn signing_keys_for(&self, origin: &ServerName) -> Result<Option<SigningKeys>>;
    fn database_version(&self) -> Result<u64>;
    fn bump_database_version(&self, new_version: u64) -> Result<()>;
}
