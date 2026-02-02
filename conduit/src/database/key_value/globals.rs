use std::collections::HashMap;

use async_trait::async_trait;
use futures_util::{stream::FuturesUnordered, StreamExt};
use lru_cache::LruCache;
use ruma::{
    api::federation::discovery::{OldVerifyKey, ServerSigningKeys},
    signatures::Ed25519KeyPair,
    DeviceId, ServerName, UserId,
};

use crate::{
    database::KeyValueDatabase,
    service::{self, globals::SigningKeys},
    services, utils, Error, Result,
};

pub const COUNTER: &[u8] = b"c";
pub const LAST_CHECK_FOR_UPDATES_COUNT: &[u8] = b"u";

#[async_trait]
impl service::globals::Data for KeyValueDatabase {
    fn next_count(&self) -> Result<u64> {
        utils::u64_from_bytes(&self.global.increment(COUNTER)?)
            .map_err(|_| Error::bad_database("Count has invalid bytes."))
    }

    fn current_count(&self) -> Result<u64> {
        self.global.get(COUNTER)?.map_or(Ok(0_u64), |bytes| {
            utils::u64_from_bytes(&bytes)
                .map_err(|_| Error::bad_database("Count has invalid bytes."))
        })
    }

    fn last_check_for_updates_id(&self) -> Result<u64> {
        self.global
            .get(LAST_CHECK_FOR_UPDATES_COUNT)?
            .map_or(Ok(0_u64), |bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("last check for updates count has invalid bytes.")
                })
            })
    }

    fn update_check_for_updates_id(&self, id: u64) -> Result<()> {
        self.global
            .insert(LAST_CHECK_FOR_UPDATES_COUNT, &id.to_be_bytes())?;

        Ok(())
    }

    async fn watch(&self, user_id: &UserId, device_id: &DeviceId) -> Result<()> {
        let userid_bytes = user_id.as_bytes().to_vec();
        let mut userid_prefix = userid_bytes.clone();
        userid_prefix.push(0xff);

        let mut userdeviceid_prefix = userid_prefix.clone();
        userdeviceid_prefix.extend_from_slice(device_id.as_bytes());
        userdeviceid_prefix.push(0xff);

        let mut futures = FuturesUnordered::new();

        // Return when *any* user changed his key
        // TODO: only send for user they share a room with
        futures.push(self.todeviceid_events.watch_prefix(&userdeviceid_prefix));

        futures.push(self.userroomid_joined.watch_prefix(&userid_prefix));
        futures.push(self.userroomid_invitestate.watch_prefix(&userid_prefix));
        futures.push(self.userroomid_leftstate.watch_prefix(&userid_prefix));
        futures.push(
            self.userroomid_notificationcount
                .watch_prefix(&userid_prefix),
        );
        futures.push(self.userroomid_highlightcount.watch_prefix(&userid_prefix));

        // Events for rooms we are in
        for room_id in services()
            .rooms
            .state_cache
            .rooms_joined(user_id)
            .filter_map(|r| r.ok())
        {
            let short_roomid = services()
                .rooms
                .short
                .get_shortroomid(&room_id)
                .ok()
                .flatten()
                .expect("room exists")
                .to_be_bytes()
                .to_vec();

            let roomid_bytes = room_id.as_bytes().to_vec();
            let mut roomid_prefix = roomid_bytes.clone();
            roomid_prefix.push(0xff);

            // PDUs
            futures.push(self.pduid_pdu.watch_prefix(&short_roomid));

            // EDUs
            futures.push(Box::into_pin(Box::new(async move {
                let _result = services().rooms.edus.typing.wait_for_update(&room_id).await;
            })));

            futures.push(self.readreceiptid_readreceipt.watch_prefix(&roomid_prefix));

            // Key changes
            futures.push(self.keychangeid_userid.watch_prefix(&roomid_prefix));

            // Room account data
            let mut roomuser_prefix = roomid_prefix.clone();
            roomuser_prefix.extend_from_slice(&userid_prefix);

            futures.push(
                self.roomusertype_roomuserdataid
                    .watch_prefix(&roomuser_prefix),
            );
        }

        let mut globaluserdata_prefix = vec![0xff];
        globaluserdata_prefix.extend_from_slice(&userid_prefix);

        futures.push(
            self.roomusertype_roomuserdataid
                .watch_prefix(&globaluserdata_prefix),
        );

        // More key changes (used when user is not joined to any rooms)
        futures.push(self.keychangeid_userid.watch_prefix(&userid_prefix));

        // One time keys
        futures.push(self.userid_lastonetimekeyupdate.watch_prefix(&userid_bytes));

        futures.push(Box::pin(services().globals.rotate.watch()));

        // Wait until one of them finds something
        futures.next().await;

        Ok(())
    }

    fn cleanup(&self) -> Result<()> {
        self._db.cleanup()
    }

    fn memory_usage(&self) -> String {
        let pdu_cache = self.pdu_cache.lock().unwrap().len();
        let shorteventid_cache = self.shorteventid_cache.lock().unwrap().len();
        let auth_chain_cache = self.auth_chain_cache.lock().unwrap().len();
        let eventidshort_cache = self.eventidshort_cache.lock().unwrap().len();
        let statekeyshort_cache = self.statekeyshort_cache.lock().unwrap().len();
        let our_real_users_cache = self.our_real_users_cache.read().unwrap().len();
        let appservice_in_room_cache = self.appservice_in_room_cache.read().unwrap().len();
        let lasttimelinecount_cache = self.lasttimelinecount_cache.lock().unwrap().len();

        let mut response = format!(
            "\
pdu_cache: {pdu_cache}
shorteventid_cache: {shorteventid_cache}
auth_chain_cache: {auth_chain_cache}
eventidshort_cache: {eventidshort_cache}
statekeyshort_cache: {statekeyshort_cache}
our_real_users_cache: {our_real_users_cache}
appservice_in_room_cache: {appservice_in_room_cache}
lasttimelinecount_cache: {lasttimelinecount_cache}\n"
        );
        if let Ok(db_stats) = self._db.memory_usage() {
            response += &db_stats;
        }

        response
    }

    fn clear_caches(&self, amount: u32) {
        if amount > 0 {
            let c = &mut *self.pdu_cache.lock().unwrap();
            *c = LruCache::new(c.capacity());
        }
        if amount > 1 {
            let c = &mut *self.shorteventid_cache.lock().unwrap();
            *c = LruCache::new(c.capacity());
        }
        if amount > 2 {
            let c = &mut *self.auth_chain_cache.lock().unwrap();
            *c = LruCache::new(c.capacity());
        }
        if amount > 3 {
            let c = &mut *self.eventidshort_cache.lock().unwrap();
            *c = LruCache::new(c.capacity());
        }
        if amount > 4 {
            let c = &mut *self.statekeyshort_cache.lock().unwrap();
            *c = LruCache::new(c.capacity());
        }
        if amount > 5 {
            let c = &mut *self.our_real_users_cache.write().unwrap();
            *c = HashMap::new();
        }
        if amount > 6 {
            let c = &mut *self.appservice_in_room_cache.write().unwrap();
            *c = HashMap::new();
        }
        if amount > 7 {
            let c = &mut *self.lasttimelinecount_cache.lock().unwrap();
            *c = HashMap::new();
        }
    }

    fn load_keypair(&self) -> Result<Ed25519KeyPair> {
        let keypair_bytes = self.global.get(b"keypair")?.map_or_else(
            || {
                let keypair = utils::generate_keypair();
                self.global.insert(b"keypair", &keypair)?;
                Ok::<_, Error>(keypair)
            },
            |s| Ok(s.to_vec()),
        )?;

        let mut parts = keypair_bytes.splitn(2, |&b| b == 0xff);

        utils::string_from_bytes(
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
        })
    }
    fn remove_keypair(&self) -> Result<()> {
        self.global.remove(b"keypair")
    }

    fn add_signing_key_from_trusted_server(
        &self,
        origin: &ServerName,
        new_keys: ServerSigningKeys,
    ) -> Result<SigningKeys> {
        let prev_keys = self.server_signingkeys.get(origin.as_bytes())?;

        Ok(
            if let Some(mut prev_keys) =
                prev_keys.and_then(|keys| serde_json::from_slice::<ServerSigningKeys>(&keys).ok())
            {
                let ServerSigningKeys {
                    verify_keys,
                    old_verify_keys,
                    valid_until_ts,
                    ..
                } = new_keys;

                prev_keys.verify_keys.extend(verify_keys);
                prev_keys.old_verify_keys.extend(old_verify_keys);

                if valid_until_ts > prev_keys.valid_until_ts {
                    prev_keys.valid_until_ts = valid_until_ts;
                }

                self.server_signingkeys.insert(
                    origin.as_bytes(),
                    &serde_json::to_vec(&prev_keys).expect("serversigningkeys can be serialized"),
                )?;

                prev_keys.into()
            } else {
                self.server_signingkeys.insert(
                    origin.as_bytes(),
                    &serde_json::to_vec(&new_keys).expect("serversigningkeys can be serialized"),
                )?;

                new_keys.into()
            },
        )
    }

    fn add_signing_key_from_origin(
        &self,
        origin: &ServerName,
        new_keys: ServerSigningKeys,
    ) -> Result<SigningKeys> {
        let prev_keys = self.server_signingkeys.get(origin.as_bytes())?;

        Ok(
            if let Some(mut prev_keys) =
                prev_keys.and_then(|keys| serde_json::from_slice::<ServerSigningKeys>(&keys).ok())
            {
                let ServerSigningKeys {
                    verify_keys,
                    old_verify_keys,
                    ..
                } = new_keys;

                // Moving `verify_keys` no longer present to `old_verify_keys`
                for (key_id, key) in prev_keys.verify_keys {
                    if !verify_keys.contains_key(&key_id) {
                        prev_keys
                            .old_verify_keys
                            .insert(key_id, OldVerifyKey::new(prev_keys.valid_until_ts, key.key));
                    }
                }

                prev_keys.verify_keys = verify_keys;
                prev_keys.old_verify_keys.extend(old_verify_keys);
                prev_keys.valid_until_ts = new_keys.valid_until_ts;

                self.server_signingkeys.insert(
                    origin.as_bytes(),
                    &serde_json::to_vec(&prev_keys).expect("serversigningkeys can be serialized"),
                )?;

                prev_keys.into()
            } else {
                self.server_signingkeys.insert(
                    origin.as_bytes(),
                    &serde_json::to_vec(&new_keys).expect("serversigningkeys can be serialized"),
                )?;

                new_keys.into()
            },
        )
    }

    /// This returns an empty `Ok(BTreeMap<..>)` when there are no keys found for the server.
    fn signing_keys_for(&self, origin: &ServerName) -> Result<Option<SigningKeys>> {
        let signingkeys = self
            .server_signingkeys
            .get(origin.as_bytes())?
            .and_then(|bytes| serde_json::from_slice::<SigningKeys>(&bytes).ok());

        Ok(signingkeys)
    }

    fn database_version(&self) -> Result<u64> {
        self.global.get(b"version")?.map_or(Ok(0), |version| {
            utils::u64_from_bytes(&version)
                .map_err(|_| Error::bad_database("Database version id is invalid."))
        })
    }

    fn bump_database_version(&self, new_version: u64) -> Result<()> {
        self.global.insert(b"version", &new_version.to_be_bytes())?;
        Ok(())
    }
}
