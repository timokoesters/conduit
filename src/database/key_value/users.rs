use std::{collections::BTreeMap, mem::size_of};

use ruma::{
    api::client::{device::Device, error::ErrorKind, filter::IncomingFilterDefinition},
    encryption::{CrossSigningKey, DeviceKeys, OneTimeKey},
    events::{AnyToDeviceEvent, StateEventType},
    serde::Raw,
    DeviceId, DeviceKeyAlgorithm, DeviceKeyId, MilliSecondsSinceUnixEpoch, MxcUri,
    UInt, UserId,
};
use tracing::warn;

use crate::{
    database::KeyValueDatabase,
    service::{self, users::clean_signatures},
    services, utils, Error, Result,
};

impl service::users::Data for KeyValueDatabase {
    /// Check if a user has an account on this homeserver.
    fn exists(&self, user_id: &UserId) -> Result<bool> {
        Ok(self.userid_password.get(user_id.as_bytes())?.is_some())
    }

    /// Check if account is deactivated
    fn is_deactivated(&self, user_id: &UserId) -> Result<bool> {
        Ok(self
            .userid_password
            .get(user_id.as_bytes())?
            .ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "User does not exist.",
            ))?
            .is_empty())
    }

    /// Returns the number of users registered on this server.
    fn count(&self) -> Result<usize> {
        Ok(self.userid_password.iter().count())
    }

    /// Find out which user an access token belongs to.
    fn find_from_token(&self, token: &str) -> Result<Option<(Box<UserId>, String)>> {
        self.token_userdeviceid
            .get(token.as_bytes())?
            .map_or(Ok(None), |bytes| {
                let mut parts = bytes.split(|&b| b == 0xff);
                let user_bytes = parts.next().ok_or_else(|| {
                    Error::bad_database("User ID in token_userdeviceid is invalid.")
                })?;
                let device_bytes = parts.next().ok_or_else(|| {
                    Error::bad_database("Device ID in token_userdeviceid is invalid.")
                })?;

                Ok(Some((
                    UserId::parse(utils::string_from_bytes(user_bytes).map_err(|_| {
                        Error::bad_database("User ID in token_userdeviceid is invalid unicode.")
                    })?)
                    .map_err(|_| {
                        Error::bad_database("User ID in token_userdeviceid is invalid.")
                    })?,
                    utils::string_from_bytes(device_bytes).map_err(|_| {
                        Error::bad_database("Device ID in token_userdeviceid is invalid.")
                    })?,
                )))
            })
    }

    /// Returns an iterator over all users on this homeserver.
    fn iter(&self) -> Box<dyn Iterator<Item = Result<Box<UserId>>>> {
        Box::new(self.userid_password.iter().map(|(bytes, _)| {
            UserId::parse(utils::string_from_bytes(&bytes).map_err(|_| {
                Error::bad_database("User ID in userid_password is invalid unicode.")
            })?)
            .map_err(|_| Error::bad_database("User ID in userid_password is invalid."))
        }))
    }

    /// Returns a list of local users as list of usernames.
    ///
    /// A user account is considered `local` if the length of it's password is greater then zero.
    fn list_local_users(&self) -> Result<Vec<String>> {
        let users: Vec<String> = self
            .userid_password
            .iter()
            .filter_map(|(username, pw)| self.get_username_with_valid_password(&username, &pw))
            .collect();
        Ok(users)
    }

    /// Will only return with Some(username) if the password was not empty and the
    /// username could be successfully parsed.
    /// If utils::string_from_bytes(...) returns an error that username will be skipped
    /// and the error will be logged.
    fn get_username_with_valid_password(&self, username: &[u8], password: &[u8]) -> Option<String> {
        // A valid password is not empty
        if password.is_empty() {
            None
        } else {
            match utils::string_from_bytes(username) {
                Ok(u) => Some(u),
                Err(e) => {
                    warn!(
                        "Failed to parse username while calling get_local_users(): {}",
                        e.to_string()
                    );
                    None
                }
            }
        }
    }

    /// Returns the password hash for the given user.
    fn password_hash(&self, user_id: &UserId) -> Result<Option<String>> {
        self.userid_password
            .get(user_id.as_bytes())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Password hash in db is not valid string.")
                })?))
            })
    }

    /// Hash and set the user's password to the Argon2 hash
    fn set_password(&self, user_id: &UserId, password: Option<&str>) -> Result<()> {
        if let Some(password) = password {
            if let Ok(hash) = utils::calculate_password_hash(password) {
                self.userid_password
                    .insert(user_id.as_bytes(), hash.as_bytes())?;
                Ok(())
            } else {
                Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Password does not meet the requirements.",
                ))
            }
        } else {
            self.userid_password.insert(user_id.as_bytes(), b"")?;
            Ok(())
        }
    }

    /// Returns the displayname of a user on this homeserver.
    fn displayname(&self, user_id: &UserId) -> Result<Option<String>> {
        self.userid_displayname
            .get(user_id.as_bytes())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Displayname in db is invalid.")
                })?))
            })
    }

    /// Sets a new displayname or removes it if displayname is None. You still need to nofify all rooms of this change.
    fn set_displayname(&self, user_id: &UserId, displayname: Option<String>) -> Result<()> {
        if let Some(displayname) = displayname {
            self.userid_displayname
                .insert(user_id.as_bytes(), displayname.as_bytes())?;
        } else {
            self.userid_displayname.remove(user_id.as_bytes())?;
        }

        Ok(())
    }

    /// Get the avatar_url of a user.
    fn avatar_url(&self, user_id: &UserId) -> Result<Option<Box<MxcUri>>> {
        self.userid_avatarurl
            .get(user_id.as_bytes())?
            .map(|bytes| {
                let s = utils::string_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Avatar URL in db is invalid."))?;
                s.try_into()
                    .map_err(|_| Error::bad_database("Avatar URL in db is invalid."))
            })
            .transpose()
    }

    /// Sets a new avatar_url or removes it if avatar_url is None.
    fn set_avatar_url(&self, user_id: &UserId, avatar_url: Option<Box<MxcUri>>) -> Result<()> {
        if let Some(avatar_url) = avatar_url {
            self.userid_avatarurl
                .insert(user_id.as_bytes(), avatar_url.to_string().as_bytes())?;
        } else {
            self.userid_avatarurl.remove(user_id.as_bytes())?;
        }

        Ok(())
    }

    /// Get the blurhash of a user.
    fn blurhash(&self, user_id: &UserId) -> Result<Option<String>> {
        self.userid_blurhash
            .get(user_id.as_bytes())?
            .map(|bytes| {
                let s = utils::string_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Avatar URL in db is invalid."))?;

                Ok(s)
            })
            .transpose()
    }

    /// Sets a new avatar_url or removes it if avatar_url is None.
    fn set_blurhash(&self, user_id: &UserId, blurhash: Option<String>) -> Result<()> {
        if let Some(blurhash) = blurhash {
            self.userid_blurhash
                .insert(user_id.as_bytes(), blurhash.as_bytes())?;
        } else {
            self.userid_blurhash.remove(user_id.as_bytes())?;
        }

        Ok(())
    }

    /// Adds a new device to a user.
    fn create_device(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        token: &str,
        initial_device_display_name: Option<String>,
    ) -> Result<()> {
        // This method should never be called for nonexistent users.
        assert!(self.exists(user_id)?);

        let mut userdeviceid = user_id.as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        self.userid_devicelistversion
            .increment(user_id.as_bytes())?;

        self.userdeviceid_metadata.insert(
            &userdeviceid,
            &serde_json::to_vec(&Device {
                device_id: device_id.into(),
                display_name: initial_device_display_name,
                last_seen_ip: None, // TODO
                last_seen_ts: Some(MilliSecondsSinceUnixEpoch::now()),
            })
            .expect("Device::to_string never fails."),
        )?;

        self.set_token(user_id, device_id, token)?;

        Ok(())
    }

    /// Removes a device from a user.
    fn remove_device(&self, user_id: &UserId, device_id: &DeviceId) -> Result<()> {
        let mut userdeviceid = user_id.as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        // Remove tokens
        if let Some(old_token) = self.userdeviceid_token.get(&userdeviceid)? {
            self.userdeviceid_token.remove(&userdeviceid)?;
            self.token_userdeviceid.remove(&old_token)?;
        }

        // Remove todevice events
        let mut prefix = userdeviceid.clone();
        prefix.push(0xff);

        for (key, _) in self.todeviceid_events.scan_prefix(prefix) {
            self.todeviceid_events.remove(&key)?;
        }

        // TODO: Remove onetimekeys

        self.userid_devicelistversion
            .increment(user_id.as_bytes())?;

        self.userdeviceid_metadata.remove(&userdeviceid)?;

        Ok(())
    }

    /// Returns an iterator over all device ids of this user.
    fn all_device_ids<'a>(
        &'a self,
        user_id: &UserId,
    ) -> Box<dyn Iterator<Item = Result<Box<DeviceId>>>> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);
        // All devices have metadata
        Box::new(
            self.userdeviceid_metadata
                .scan_prefix(prefix)
                .map(|(bytes, _)| {
                    Ok(utils::string_from_bytes(
                        bytes.rsplit(|&b| b == 0xff).next().ok_or_else(|| {
                            Error::bad_database("UserDevice ID in db is invalid.")
                        })?,
                    )
                    .map_err(|_| {
                        Error::bad_database("Device ID in userdeviceid_metadata is invalid.")
                    })?
                    .into())
                }),
        )
    }

    /// Replaces the access token of one device.
    fn set_token(&self, user_id: &UserId, device_id: &DeviceId, token: &str) -> Result<()> {
        let mut userdeviceid = user_id.as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        // All devices have metadata
        assert!(self.userdeviceid_metadata.get(&userdeviceid)?.is_some());

        // Remove old token
        if let Some(old_token) = self.userdeviceid_token.get(&userdeviceid)? {
            self.token_userdeviceid.remove(&old_token)?;
            // It will be removed from userdeviceid_token by the insert later
        }

        // Assign token to user device combination
        self.userdeviceid_token
            .insert(&userdeviceid, token.as_bytes())?;
        self.token_userdeviceid
            .insert(token.as_bytes(), &userdeviceid)?;

        Ok(())
    }

    fn add_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        one_time_key_key: &DeviceKeyId,
        one_time_key_value: &Raw<OneTimeKey>,
    ) -> Result<()> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.as_bytes());

        // All devices have metadata
        // Only existing devices should be able to call this.
        assert!(self.userdeviceid_metadata.get(&key)?.is_some());

        key.push(0xff);
        // TODO: Use DeviceKeyId::to_string when it's available (and update everything,
        // because there are no wrapping quotation marks anymore)
        key.extend_from_slice(
            serde_json::to_string(one_time_key_key)
                .expect("DeviceKeyId::to_string always works")
                .as_bytes(),
        );

        self.onetimekeyid_onetimekeys.insert(
            &key,
            &serde_json::to_vec(&one_time_key_value).expect("OneTimeKey::to_vec always works"),
        )?;

        self.userid_lastonetimekeyupdate.insert(
            user_id.as_bytes(),
            &services().globals.next_count()?.to_be_bytes(),
        )?;

        Ok(())
    }

    fn last_one_time_keys_update(&self, user_id: &UserId) -> Result<u64> {
        self.userid_lastonetimekeyupdate
            .get(user_id.as_bytes())?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Count in roomid_lastroomactiveupdate is invalid.")
                })
            })
            .unwrap_or(Ok(0))
    }

    fn take_one_time_key(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        key_algorithm: &DeviceKeyAlgorithm,
    ) -> Result<Option<(Box<DeviceKeyId>, Raw<OneTimeKey>)>> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(device_id.as_bytes());
        prefix.push(0xff);
        prefix.push(b'"'); // Annoying quotation mark
        prefix.extend_from_slice(key_algorithm.as_ref().as_bytes());
        prefix.push(b':');

        self.userid_lastonetimekeyupdate.insert(
            user_id.as_bytes(),
            &services().globals.next_count()?.to_be_bytes(),
        )?;

        self.onetimekeyid_onetimekeys
            .scan_prefix(prefix)
            .next()
            .map(|(key, value)| {
                self.onetimekeyid_onetimekeys.remove(&key)?;

                Ok((
                    serde_json::from_slice(
                        &*key
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .ok_or_else(|| Error::bad_database("OneTimeKeyId in db is invalid."))?,
                    )
                    .map_err(|_| Error::bad_database("OneTimeKeyId in db is invalid."))?,
                    serde_json::from_slice(&*value)
                        .map_err(|_| Error::bad_database("OneTimeKeys in db are invalid."))?,
                ))
            })
            .transpose()
    }

    fn count_one_time_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<BTreeMap<DeviceKeyAlgorithm, UInt>> {
        let mut userdeviceid = user_id.as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        let mut counts = BTreeMap::new();

        for algorithm in
            self.onetimekeyid_onetimekeys
                .scan_prefix(userdeviceid)
                .map(|(bytes, _)| {
                    Ok::<_, Error>(
                        serde_json::from_slice::<Box<DeviceKeyId>>(
                            &*bytes.rsplit(|&b| b == 0xff).next().ok_or_else(|| {
                                Error::bad_database("OneTimeKey ID in db is invalid.")
                            })?,
                        )
                        .map_err(|_| Error::bad_database("DeviceKeyId in db is invalid."))?
                        .algorithm(),
                    )
                })
        {
            *counts.entry(algorithm?).or_default() += UInt::from(1_u32);
        }

        Ok(counts)
    }

    fn add_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        device_keys: &Raw<DeviceKeys>,
    ) -> Result<()> {
        let mut userdeviceid = user_id.as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        self.keyid_key.insert(
            &userdeviceid,
            &serde_json::to_vec(&device_keys).expect("DeviceKeys::to_vec always works"),
        )?;

        self.mark_device_key_update(user_id)?;

        Ok(())
    }

    fn add_cross_signing_keys(
        &self,
        user_id: &UserId,
        master_key: &Raw<CrossSigningKey>,
        self_signing_key: &Option<Raw<CrossSigningKey>>,
        user_signing_key: &Option<Raw<CrossSigningKey>>,
    ) -> Result<()> {
        // TODO: Check signatures

        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        // Master key
        let mut master_key_ids = master_key
            .deserialize()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid master key"))?
            .keys
            .into_values();

        let master_key_id = master_key_ids.next().ok_or(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Master key contained no key.",
        ))?;

        if master_key_ids.next().is_some() {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Master key contained more than one key.",
            ));
        }

        let mut master_key_key = prefix.clone();
        master_key_key.extend_from_slice(master_key_id.as_bytes());

        self.keyid_key
            .insert(&master_key_key, master_key.json().get().as_bytes())?;

        self.userid_masterkeyid
            .insert(user_id.as_bytes(), &master_key_key)?;

        // Self-signing key
        if let Some(self_signing_key) = self_signing_key {
            let mut self_signing_key_ids = self_signing_key
                .deserialize()
                .map_err(|_| {
                    Error::BadRequest(ErrorKind::InvalidParam, "Invalid self signing key")
                })?
                .keys
                .into_values();

            let self_signing_key_id = self_signing_key_ids.next().ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Self signing key contained no key.",
            ))?;

            if self_signing_key_ids.next().is_some() {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Self signing key contained more than one key.",
                ));
            }

            let mut self_signing_key_key = prefix.clone();
            self_signing_key_key.extend_from_slice(self_signing_key_id.as_bytes());

            self.keyid_key.insert(
                &self_signing_key_key,
                self_signing_key.json().get().as_bytes(),
            )?;

            self.userid_selfsigningkeyid
                .insert(user_id.as_bytes(), &self_signing_key_key)?;
        }

        // User-signing key
        if let Some(user_signing_key) = user_signing_key {
            let mut user_signing_key_ids = user_signing_key
                .deserialize()
                .map_err(|_| {
                    Error::BadRequest(ErrorKind::InvalidParam, "Invalid user signing key")
                })?
                .keys
                .into_values();

            let user_signing_key_id = user_signing_key_ids.next().ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "User signing key contained no key.",
            ))?;

            if user_signing_key_ids.next().is_some() {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "User signing key contained more than one key.",
                ));
            }

            let mut user_signing_key_key = prefix;
            user_signing_key_key.extend_from_slice(user_signing_key_id.as_bytes());

            self.keyid_key.insert(
                &user_signing_key_key,
                user_signing_key.json().get().as_bytes(),
            )?;

            self.userid_usersigningkeyid
                .insert(user_id.as_bytes(), &user_signing_key_key)?;
        }

        self.mark_device_key_update(user_id)?;

        Ok(())
    }

    fn sign_key(
        &self,
        target_id: &UserId,
        key_id: &str,
        signature: (String, String),
        sender_id: &UserId,
    ) -> Result<()> {
        let mut key = target_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(key_id.as_bytes());

        let mut cross_signing_key: serde_json::Value =
            serde_json::from_slice(&self.keyid_key.get(&key)?.ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Tried to sign nonexistent key.",
            ))?)
            .map_err(|_| Error::bad_database("key in keyid_key is invalid."))?;

        let signatures = cross_signing_key
            .get_mut("signatures")
            .ok_or_else(|| Error::bad_database("key in keyid_key has no signatures field."))?
            .as_object_mut()
            .ok_or_else(|| Error::bad_database("key in keyid_key has invalid signatures field."))?
            .entry(sender_id.to_owned())
            .or_insert_with(|| serde_json::Map::new().into());

        signatures
            .as_object_mut()
            .ok_or_else(|| Error::bad_database("signatures in keyid_key for a user is invalid."))?
            .insert(signature.0, signature.1.into());

        self.keyid_key.insert(
            &key,
            &serde_json::to_vec(&cross_signing_key).expect("CrossSigningKey::to_vec always works"),
        )?;

        // TODO: Should we notify about this change?
        self.mark_device_key_update(target_id)?;

        Ok(())
    }

    fn keys_changed<'a>(
        &'a self,
        user_or_room_id: &str,
        from: u64,
        to: Option<u64>,
    ) -> Box<dyn Iterator<Item = Result<Box<UserId>>>> {
        let mut prefix = user_or_room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut start = prefix.clone();
        start.extend_from_slice(&(from + 1).to_be_bytes());

        let to = to.unwrap_or(u64::MAX);

        Box::new(
            self.keychangeid_userid
                .iter_from(&start, false)
                .take_while(move |(k, _)| {
                    k.starts_with(&prefix)
                        && if let Some(current) = k.splitn(2, |&b| b == 0xff).nth(1) {
                            if let Ok(c) = utils::u64_from_bytes(current) {
                                c <= to
                            } else {
                                warn!("BadDatabase: Could not parse keychangeid_userid bytes");
                                false
                            }
                        } else {
                            warn!("BadDatabase: Could not parse keychangeid_userid");
                            false
                        }
                })
                .map(|(_, bytes)| {
                    UserId::parse(utils::string_from_bytes(&bytes).map_err(|_| {
                        Error::bad_database(
                            "User ID in devicekeychangeid_userid is invalid unicode.",
                        )
                    })?)
                    .map_err(|_| {
                        Error::bad_database("User ID in devicekeychangeid_userid is invalid.")
                    })
                }),
        )
    }

    fn mark_device_key_update(&self, user_id: &UserId) -> Result<()> {
        let count = services().globals.next_count()?.to_be_bytes();
        for room_id in services()
            .rooms
            .state_cache
            .rooms_joined(user_id)
            .filter_map(|r| r.ok())
        {
            // Don't send key updates to unencrypted rooms
            if services()
                .rooms
                .state_accessor
                .room_state_get(&room_id, &StateEventType::RoomEncryption, "")?
                .is_none()
            {
                continue;
            }

            let mut key = room_id.as_bytes().to_vec();
            key.push(0xff);
            key.extend_from_slice(&count);

            self.keychangeid_userid.insert(&key, user_id.as_bytes())?;
        }

        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&count);
        self.keychangeid_userid.insert(&key, user_id.as_bytes())?;

        Ok(())
    }

    fn get_device_keys(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Raw<DeviceKeys>>> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.as_bytes());

        self.keyid_key.get(&key)?.map_or(Ok(None), |bytes| {
            Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
                Error::bad_database("DeviceKeys in db are invalid.")
            })?))
        })
    }

    fn get_master_key(
        &self,
        user_id: &UserId,
        allowed_signatures: &dyn Fn(&UserId) -> bool,
    ) -> Result<Option<Raw<CrossSigningKey>>> {
        self.userid_masterkeyid
            .get(user_id.as_bytes())?
            .map_or(Ok(None), |key| {
                self.keyid_key.get(&key)?.map_or(Ok(None), |bytes| {
                    let mut cross_signing_key = serde_json::from_slice::<serde_json::Value>(&bytes)
                        .map_err(|_| Error::bad_database("CrossSigningKey in db is invalid."))?;
                    clean_signatures(&mut cross_signing_key, user_id, allowed_signatures)?;

                    Ok(Some(Raw::from_json(
                        serde_json::value::to_raw_value(&cross_signing_key)
                            .expect("Value to RawValue serialization"),
                    )))
                })
            })
    }

    fn get_self_signing_key(
        &self,
        user_id: &UserId,
        allowed_signatures: &dyn Fn(&UserId) -> bool,
    ) -> Result<Option<Raw<CrossSigningKey>>> {
        self.userid_selfsigningkeyid
            .get(user_id.as_bytes())?
            .map_or(Ok(None), |key| {
                self.keyid_key.get(&key)?.map_or(Ok(None), |bytes| {
                    let mut cross_signing_key = serde_json::from_slice::<serde_json::Value>(&bytes)
                        .map_err(|_| Error::bad_database("CrossSigningKey in db is invalid."))?;
                    clean_signatures(&mut cross_signing_key, user_id, allowed_signatures)?;

                    Ok(Some(Raw::from_json(
                        serde_json::value::to_raw_value(&cross_signing_key)
                            .expect("Value to RawValue serialization"),
                    )))
                })
            })
    }

    fn get_user_signing_key(&self, user_id: &UserId) -> Result<Option<Raw<CrossSigningKey>>> {
        self.userid_usersigningkeyid
            .get(user_id.as_bytes())?
            .map_or(Ok(None), |key| {
                self.keyid_key.get(&key)?.map_or(Ok(None), |bytes| {
                    Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
                        Error::bad_database("CrossSigningKey in db is invalid.")
                    })?))
                })
            })
    }

    fn add_to_device_event(
        &self,
        sender: &UserId,
        target_user_id: &UserId,
        target_device_id: &DeviceId,
        event_type: &str,
        content: serde_json::Value,
    ) -> Result<()> {
        let mut key = target_user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(target_device_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(&services().globals.next_count()?.to_be_bytes());

        let mut json = serde_json::Map::new();
        json.insert("type".to_owned(), event_type.to_owned().into());
        json.insert("sender".to_owned(), sender.to_string().into());
        json.insert("content".to_owned(), content);

        let value = serde_json::to_vec(&json).expect("Map::to_vec always works");

        self.todeviceid_events.insert(&key, &value)?;

        Ok(())
    }

    fn get_to_device_events(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Vec<Raw<AnyToDeviceEvent>>> {
        let mut events = Vec::new();

        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(device_id.as_bytes());
        prefix.push(0xff);

        for (_, value) in self.todeviceid_events.scan_prefix(prefix) {
            events.push(
                serde_json::from_slice(&value)
                    .map_err(|_| Error::bad_database("Event in todeviceid_events is invalid."))?,
            );
        }

        Ok(events)
    }

    fn remove_to_device_events(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        until: u64,
    ) -> Result<()> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(device_id.as_bytes());
        prefix.push(0xff);

        let mut last = prefix.clone();
        last.extend_from_slice(&until.to_be_bytes());

        for (key, _) in self
            .todeviceid_events
            .iter_from(&last, true) // this includes last
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(|(key, _)| {
                Ok::<_, Error>((
                    key.clone(),
                    utils::u64_from_bytes(&key[key.len() - size_of::<u64>()..key.len()])
                        .map_err(|_| Error::bad_database("ToDeviceId has invalid count bytes."))?,
                ))
            })
            .filter_map(|r| r.ok())
            .take_while(|&(_, count)| count <= until)
        {
            self.todeviceid_events.remove(&key)?;
        }

        Ok(())
    }

    fn update_device_metadata(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        device: &Device,
    ) -> Result<()> {
        let mut userdeviceid = user_id.as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        // Only existing devices should be able to call this.
        assert!(self.userdeviceid_metadata.get(&userdeviceid)?.is_some());

        self.userid_devicelistversion
            .increment(user_id.as_bytes())?;

        self.userdeviceid_metadata.insert(
            &userdeviceid,
            &serde_json::to_vec(device).expect("Device::to_string always works"),
        )?;

        Ok(())
    }

    /// Get device metadata.
    fn get_device_metadata(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
    ) -> Result<Option<Device>> {
        let mut userdeviceid = user_id.as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        self.userdeviceid_metadata
            .get(&userdeviceid)?
            .map_or(Ok(None), |bytes| {
                Ok(Some(serde_json::from_slice(&bytes).map_err(|_| {
                    Error::bad_database("Metadata in userdeviceid_metadata is invalid.")
                })?))
            })
    }

    fn get_devicelist_version(&self, user_id: &UserId) -> Result<Option<u64>> {
        self.userid_devicelistversion
            .get(user_id.as_bytes())?
            .map_or(Ok(None), |bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid devicelistversion in db."))
                    .map(Some)
            })
    }

    fn all_devices_metadata<'a>(
        &'a self,
        user_id: &UserId,
    ) -> Box<dyn Iterator<Item = Result<Device>>> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);

        Box::new(
            self.userdeviceid_metadata
                .scan_prefix(key)
                .map(|(_, bytes)| {
                    serde_json::from_slice::<Device>(&bytes).map_err(|_| {
                        Error::bad_database("Device in userdeviceid_metadata is invalid.")
                    })
                }),
        )
    }

    /// Creates a new sync filter. Returns the filter id.
    fn create_filter(&self, user_id: &UserId, filter: &IncomingFilterDefinition) -> Result<String> {
        let filter_id = utils::random_string(4);

        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(filter_id.as_bytes());

        self.userfilterid_filter.insert(
            &key,
            &serde_json::to_vec(&filter).expect("filter is valid json"),
        )?;

        Ok(filter_id)
    }

    fn get_filter(
        &self,
        user_id: &UserId,
        filter_id: &str,
    ) -> Result<Option<IncomingFilterDefinition>> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(filter_id.as_bytes());

        let raw = self.userfilterid_filter.get(&key)?;

        if let Some(raw) = raw {
            serde_json::from_slice(&raw)
                .map_err(|_| Error::bad_database("Invalid filter event in db."))
        } else {
            Ok(None)
        }
    }
}
