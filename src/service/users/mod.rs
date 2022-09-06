mod data;
use std::{collections::BTreeMap, mem};

pub use data::Data;
use ruma::{UserId, MxcUri, DeviceId, DeviceKeyId, serde::Raw, encryption::{OneTimeKey, CrossSigningKey, DeviceKeys}, DeviceKeyAlgorithm, UInt, events::AnyToDeviceEvent, api::client::{device::Device, filter::IncomingFilterDefinition}};

use crate::{service::*, Error};

pub struct Service<D: Data> {
    db: D,
}

impl Service<_> {
    /// Check if a user has an account on this homeserver.
    pub fn exists(&self, user_id: &UserId) -> Result<bool> {
        self.db.exists(user_id)
    }

    /// Check if account is deactivated
    pub fn is_deactivated(&self, user_id: &UserId) -> Result<bool> {
        self.db.is_deactivated(user_id)
    }

    /// Check if a user is an admin
    fn is_admin(
        &self,
        user_id: &UserId,
    ) -> Result<bool> {
        let admin_room_alias_id = RoomAliasId::parse(format!("#admins:{}", globals.server_name()))
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid alias."))?;
        let admin_room_id = rooms.id_from_alias(&admin_room_alias_id)?.unwrap();

        rooms.is_joined(user_id, &admin_room_id)
    }

    /// Create a new user account on this homeserver.
    fn create(&self, user_id: &UserId, password: Option<&str>) -> Result<()> {
        self.db.set_password(user_id, password)?;
        Ok(())
    }


    /// Returns the number of users registered on this server.
    pub fn count(&self) -> Result<usize> {
        self.db.count()
    }

    /// Find out which user an access token belongs to.
    pub fn find_from_token(&self, token: &str) -> Result<Option<(Box<UserId>, String)>> {
        self.db.find_from_token(token)
    }

    /// Returns an iterator over all users on this homeserver.
    pub fn iter(&self) -> impl Iterator<Item = Result<Box<UserId>>> + '_ {
        self.db.iter()
    }

    /// Returns a list of local users as list of usernames.
    ///
    /// A user account is considered `local` if the length of it's password is greater then zero.
    pub fn list_local_users(&self) -> Result<Vec<String>> {
        self.db.list_local_users()
    }

    /// Will only return with Some(username) if the password was not empty and the
    /// username could be successfully parsed.
    /// If utils::string_from_bytes(...) returns an error that username will be skipped
    /// and the error will be logged.
    fn get_username_with_valid_password(&self, username: &[u8], password: &[u8]) -> Option<String> {
        self.db.get_username_with_valid_password(username, password)
    }

    /// Returns the password hash for the given user.
    pub fn password_hash(&self, user_id: &UserId) -> Result<Option<String>> {
        self.db.password_hash(user_id)
    }

    /// Hash and set the user's password to the Argon2 hash
    pub fn set_password(&self, user_id: &UserId, password: Option<&str>) -> Result<()> {
        self.db.set_password(user_id, password)
    }

    /// Returns the displayname of a user on this homeserver.
    pub fn displayname(&self, user_id: &UserId) -> Result<Option<String>> {
        self.db.displayname(user_id)
    }

    /// Sets a new displayname or removes it if displayname is None. You still need to nofify all rooms of this change.
    pub fn set_displayname(&self, user_id: &UserId, displayname: Option<String>) -> Result<()> {
        self.db.set_displayname(user_id, displayname)
    }

    /// Get the avatar_url of a user.
    pub fn avatar_url(&self, user_id: &UserId) -> Result<Option<Box<MxcUri>>> {
        self.db.avatar_url(user_id)
    }

    /// Sets a new avatar_url or removes it if avatar_url is None.
    pub fn set_avatar_url(&self, user_id: &UserId, avatar_url: Option<Box<MxcUri>>) -> Result<()> {
        self.db.set_avatar_url(user_id, avatar_url)
    }

    /// Get the blurhash of a user.
    pub fn blurhash(&self, user_id: &UserId) -> Result<Option<String>> {
        self.db.blurhash(user_id)
    }

    /// Sets a new avatar_url or removes it if avatar_url is None.
    pub fn set_blurhash(&self, user_id: &UserId, blurhash: Option<String>) -> Result<()> {
        self.db.set_blurhash(user_id, blurhash)
    }

    /// Adds a new device to a user.
    pub fn create_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        token: &str,
        initial_device_display_name: Option<String>,
    ) -> Result<()> {
        self.db.create_device(user_id, device_id, token, initial_device_display_name)
    }

    /// Removes a device from a user.
    pub fn remove_device(&self, user_id: &UserId, device_id: &DeviceId) -> Result<()> {
        self.db.remove_device(user_id, device_id)
    }

    /// Returns an iterator over all device ids of this user.
    pub fn all_device_ids<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<Box<DeviceId>>> + 'a {
        self.db.all_device_ids(user_id)
    }

    /// Replaces the access token of one device.
    pub fn set_token(&self, user_id: &UserId, device_id: &DeviceId, token: &str) -> Result<()> {
        self.db.set_token(user_id, device_id, token)
    }

    pub fn add_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        one_time_key_key: &DeviceKeyId,
        one_time_key_value: &Raw<OneTimeKey>,
    ) -> Result<()> {
        self.db.add_one_time_key(user_id, device_id, one_time_key_key, one_time_key_value)
    }

    pub fn last_one_time_keys_update(&self, user_id: &UserId) -> Result<u64> {
        self.db.last_one_time_keys_update(user_id)
    }

    pub fn take_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        key_algorithm: &DeviceKeyAlgorithm,
    ) -> Result<Option<(Box<DeviceKeyId>, Raw<OneTimeKey>)>> {
        self.db.take_one_time_key(user_id, device_id, key_algorithm)
    }

    pub fn count_one_time_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<BTreeMap<DeviceKeyAlgorithm, UInt>> {
        self.db.count_one_time_keys(user_id, device_id)
    }

    pub fn add_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        device_keys: &Raw<DeviceKeys>,
    ) -> Result<()> {
        self.db.add_device_keys(user_id, device_id, device_keys)
    }

    pub fn add_cross_signing_keys(
        &self,
        user_id: &UserId,
        master_key: &Raw<CrossSigningKey>,
        self_signing_key: &Option<Raw<CrossSigningKey>>,
        user_signing_key: &Option<Raw<CrossSigningKey>>,
    ) -> Result<()> {
        self.db.add_cross_signing_keys(user_id, master_key, self_signing_key, user_signing_key)
    }

    pub fn sign_key(
        &self,
        target_id: &UserId,
        key_id: &str,
        signature: (String, String),
        sender_id: &UserId,
    ) -> Result<()> {
        self.db.sign_key(target_id, key_id, signature, sender_id)
    }

    pub fn keys_changed<'a>(
        &'a self,
        user_or_room_id: &str,
        from: u64,
        to: Option<u64>,
    ) -> impl Iterator<Item = Result<Box<UserId>>> + 'a {
        self.db.keys_changed(user_or_room_id, from, to)
    }

    pub fn mark_device_key_update(
        &self,
        user_id: &UserId,
    ) -> Result<()> {
        self.db.mark_device_key_update(user_id)
    }

    pub fn get_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Raw<DeviceKeys>>> {
        self.db.get_device_keys(user_id, device_id)
    }

    pub fn get_master_key<F: Fn(&UserId) -> bool>(
        &self,
        user_id: &UserId,
        allowed_signatures: F,
    ) -> Result<Option<Raw<CrossSigningKey>>> {
        self.db.get_master_key(user_id, allowed_signatures)
    }

    pub fn get_self_signing_key<F: Fn(&UserId) -> bool>(
        &self,
        user_id: &UserId,
        allowed_signatures: F,
    ) -> Result<Option<Raw<CrossSigningKey>>> {
        self.db.get_self_signing_key(user_id, allowed_signatures)
    }

    pub fn get_user_signing_key(&self, user_id: &UserId) -> Result<Option<Raw<CrossSigningKey>>> {
        self.db.get_user_signing_key(user_id)
    }

    pub fn add_to_device_event(
        &self,
        sender: &UserId,
        target_user_id: &UserId,
        target_device_id: &DeviceId,
        event_type: &str,
        content: serde_json::Value,
    ) -> Result<()> {
        self.db.add_to_device_event(sender, target_user_id, target_device_id, event_type, content)
    }

    pub fn get_to_device_events(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Vec<Raw<AnyToDeviceEvent>>> {
        self.get_to_device_events(user_id, device_id)
    }

    pub fn remove_to_device_events(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        until: u64,
    ) -> Result<()> {
        self.db.remove_to_device_events(user_id, device_id, until)
    }

    pub fn update_device_metadata(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        device: &Device,
    ) -> Result<()> {
        self.db.update_device_metadata(user_id, device_id, device)
    }

    /// Get device metadata.
    pub fn get_device_metadata(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Device>> {
        self.get_device_metadata(user_id, device_id)
    }

    pub fn get_devicelist_version(&self, user_id: &UserId) -> Result<Option<u64>> {
        self.db.devicelist_version(user_id)
    }

    pub fn all_devices_metadata<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<Device>> + 'a {
        self.db.all_devices_metadata(user_id)
    }

    /// Deactivate account
    pub fn deactivate_account(&self, user_id: &UserId) -> Result<()> {
        // Remove all associated devices
        for device_id in self.all_device_ids(user_id) {
            self.remove_device(user_id, &device_id?)?;
        }

        // Set the password to "" to indicate a deactivated account. Hashes will never result in an
        // empty string, so the user will not be able to log in again. Systems like changing the
        // password without logging in should check if the account is deactivated.
        self.userid_password.insert(user_id.as_bytes(), &[])?;

        // TODO: Unhook 3PID
        Ok(())
    }

    /// Creates a new sync filter. Returns the filter id.
    pub fn create_filter(
        &self,
        user_id: &UserId,
        filter: &IncomingFilterDefinition,
    ) -> Result<String> {
        self.db.create_filter(user_id, filter)
    }

    pub fn get_filter(
        &self,
        user_id: &UserId,
        filter_id: &str,
    ) -> Result<Option<IncomingFilterDefinition>> {
        self.db.get_filter(user_id, filter_id)
    }
}

/// Ensure that a user only sees signatures from themselves and the target user
pub fn clean_signatures<F: Fn(&UserId) -> bool>(
    cross_signing_key: &mut serde_json::Value,
    user_id: &UserId,
    allowed_signatures: F,
) -> Result<(), Error> {
    if let Some(signatures) = cross_signing_key
        .get_mut("signatures")
        .and_then(|v| v.as_object_mut())
    {
        // Don't allocate for the full size of the current signatures, but require
        // at most one resize if nothing is dropped
        let new_capacity = signatures.len() / 2;
        for (user, signature) in
            mem::replace(signatures, serde_json::Map::with_capacity(new_capacity))
        {
            let id = <&UserId>::try_from(user.as_str())
                .map_err(|_| Error::bad_database("Invalid user ID in database."))?;
            if id == user_id || allowed_signatures(id) {
                signatures.insert(user, signature);
            }
        }
    }

    Ok(())
}
