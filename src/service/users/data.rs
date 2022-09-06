use std::collections::BTreeMap;

use ruma::{UserId, DeviceId, DeviceKeyAlgorithm, DeviceKeyId, serde::Raw, encryption::{OneTimeKey, DeviceKeys, CrossSigningKey}, UInt, events::AnyToDeviceEvent, api::client::{device::Device, filter::IncomingFilterDefinition}, MxcUri};

trait Data {
    /// Check if a user has an account on this homeserver.
    fn exists(&self, user_id: &UserId) -> Result<bool>;

    /// Check if account is deactivated
    fn is_deactivated(&self, user_id: &UserId) -> Result<bool>;

    /// Returns the number of users registered on this server.
    fn count(&self) -> Result<usize>;

    /// Find out which user an access token belongs to.
    fn find_from_token(&self, token: &str) -> Result<Option<(Box<UserId>, String)>>;

    /// Returns an iterator over all users on this homeserver.
    fn iter(&self) -> impl Iterator<Item = Result<Box<UserId>>> + '_;

    /// Returns a list of local users as list of usernames.
    ///
    /// A user account is considered `local` if the length of it's password is greater then zero.
    fn list_local_users(&self) -> Result<Vec<String>>;

    /// Will only return with Some(username) if the password was not empty and the
    /// username could be successfully parsed.
    /// If utils::string_from_bytes(...) returns an error that username will be skipped
    /// and the error will be logged.
    fn get_username_with_valid_password(&self, username: &[u8], password: &[u8]) -> Option<String>;

    /// Returns the password hash for the given user.
    fn password_hash(&self, user_id: &UserId) -> Result<Option<String>>;

    /// Hash and set the user's password to the Argon2 hash
    fn set_password(&self, user_id: &UserId, password: Option<&str>) -> Result<()>;

    /// Returns the displayname of a user on this homeserver.
    fn displayname(&self, user_id: &UserId) -> Result<Option<String>>;

    /// Sets a new displayname or removes it if displayname is None. You still need to nofify all rooms of this change.
    fn set_displayname(&self, user_id: &UserId, displayname: Option<String>) -> Result<()>;

    /// Get the avatar_url of a user.
    fn avatar_url(&self, user_id: &UserId) -> Result<Option<Box<MxcUri>>>;

    /// Sets a new avatar_url or removes it if avatar_url is None.
    fn set_avatar_url(&self, user_id: &UserId, avatar_url: Option<Box<MxcUri>>) -> Result<()>;

    /// Get the blurhash of a user.
    fn blurhash(&self, user_id: &UserId) -> Result<Option<String>>;

    /// Sets a new avatar_url or removes it if avatar_url is None.
    fn set_blurhash(&self, user_id: &UserId, blurhash: Option<String>) -> Result<()>;

    /// Adds a new device to a user.
    fn create_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        token: &str,
        initial_device_display_name: Option<String>,
    ) -> Result<()>;

    /// Removes a device from a user.
    fn remove_device(&self, user_id: &UserId, device_id: &DeviceId) -> Result<()>;

    /// Returns an iterator over all device ids of this user.
    fn all_device_ids<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<Box<DeviceId>>> + 'a;

    /// Replaces the access token of one device.
    fn set_token(&self, user_id: &UserId, device_id: &DeviceId, token: &str) -> Result<()>;

    fn add_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        one_time_key_key: &DeviceKeyId,
        one_time_key_value: &Raw<OneTimeKey>,
    ) -> Result<()>;

    fn last_one_time_keys_update(&self, user_id: &UserId) -> Result<u64>;

    fn take_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        key_algorithm: &DeviceKeyAlgorithm,
    ) -> Result<Option<(Box<DeviceKeyId>, Raw<OneTimeKey>)>>;

    fn count_one_time_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<BTreeMap<DeviceKeyAlgorithm, UInt>>;

    fn add_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        device_keys: &Raw<DeviceKeys>,
    ) -> Result<()>;

    fn add_cross_signing_keys(
        &self,
        user_id: &UserId,
        master_key: &Raw<CrossSigningKey>,
        self_signing_key: &Option<Raw<CrossSigningKey>>,
        user_signing_key: &Option<Raw<CrossSigningKey>>,
    ) -> Result<()>;

    fn sign_key(
        &self,
        target_id: &UserId,
        key_id: &str,
        signature: (String, String),
        sender_id: &UserId,
    ) -> Result<()>;

    fn keys_changed<'a>(
        &'a self,
        user_or_room_id: &str,
        from: u64,
        to: Option<u64>,
    ) -> impl Iterator<Item = Result<Box<UserId>>> + 'a;

    fn mark_device_key_update(
        &self,
        user_id: &UserId,
    ) -> Result<()>;

    fn get_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Raw<DeviceKeys>>>;

    fn get_master_key<F: Fn(&UserId) -> bool>(
        &self,
        user_id: &UserId,
        allowed_signatures: F,
    ) -> Result<Option<Raw<CrossSigningKey>>>;

    fn get_self_signing_key<F: Fn(&UserId) -> bool>(
        &self,
        user_id: &UserId,
        allowed_signatures: F,
    ) -> Result<Option<Raw<CrossSigningKey>>>;

    fn get_user_signing_key(&self, user_id: &UserId) -> Result<Option<Raw<CrossSigningKey>>>;

    fn add_to_device_event(
        &self,
        sender: &UserId,
        target_user_id: &UserId,
        target_device_id: &DeviceId,
        event_type: &str,
        content: serde_json::Value,
    ) -> Result<()>;

    fn get_to_device_events(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Vec<Raw<AnyToDeviceEvent>>>;

    fn remove_to_device_events(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        until: u64,
    ) -> Result<()>;

    fn update_device_metadata(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        device: &Device,
    ) -> Result<()>;

    /// Get device metadata.
    fn get_device_metadata(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Device>>;

    fn get_devicelist_version(&self, user_id: &UserId) -> Result<Option<u64>>;

    fn all_devices_metadata<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<Device>> + 'a;

    /// Creates a new sync filter. Returns the filter id.
    fn create_filter(
        &self,
        user_id: &UserId,
        filter: &IncomingFilterDefinition,
    ) -> Result<String>;

    fn get_filter(
        &self,
        user_id: &UserId,
        filter_id: &str,
    ) -> Result<Option<IncomingFilterDefinition>>;
}
