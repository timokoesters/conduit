use std::collections::BTreeMap;

use async_trait::async_trait;
use ruma::{
    api::federation::discovery::{ServerSigningKeys, VerifyKey},
    signatures::Ed25519KeyPair,
    DeviceId, ServerName, ServerSigningKeyId, UserId,
};

use crate::Result;

#[async_trait]
pub trait Data: Send + Sync {
    fn next_count(&self) -> Result<u64>;
    fn current_count(&self) -> Result<u64>;
    async fn watch(&self, user_id: &UserId, device_id: &DeviceId) -> Result<()>;
    fn cleanup(&self) -> Result<()>;
    fn memory_usage(&self) -> Result<String>;
    fn load_keypair(&self) -> Result<Ed25519KeyPair>;
    fn remove_keypair(&self) -> Result<()>;
    fn add_signing_key(
        &self,
        origin: &ServerName,
        new_keys: ServerSigningKeys,
    ) -> Result<BTreeMap<Box<ServerSigningKeyId>, VerifyKey>>;

    /// This returns an empty `Ok(BTreeMap<..>)` when there are no keys found for the server.
    fn signing_keys_for(
        &self,
        origin: &ServerName,
    ) -> Result<BTreeMap<Box<ServerSigningKeyId>, VerifyKey>>;
    fn database_version(&self) -> Result<u64>;
    fn bump_database_version(&self, new_version: u64) -> Result<()>;
}
