use std::collections::BTreeMap;

use async_trait::async_trait;
use futures_util::{stream::FuturesUnordered, StreamExt};
use ruma::{signatures::Ed25519KeyPair, UserId, DeviceId, ServerName, api::federation::discovery::{ServerSigningKeys, VerifyKey}, ServerSigningKeyId, MilliSecondsSinceUnixEpoch};

use crate::{Result, service, database::KeyValueDatabase, Error, utils, services};

pub const COUNTER: &[u8] = b"c";

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
        futures.push(
            self.todeviceid_events
                .watch_prefix(&userdeviceid_prefix),
        );

        futures.push(self.userroomid_joined.watch_prefix(&userid_prefix));
        futures.push(
            self.userroomid_invitestate
                .watch_prefix(&userid_prefix),
        );
        futures.push(self.userroomid_leftstate.watch_prefix(&userid_prefix));
        futures.push(
            self.userroomid_notificationcount
                .watch_prefix(&userid_prefix),
        );
        futures.push(
            self.userroomid_highlightcount
                .watch_prefix(&userid_prefix),
        );

        // Events for rooms we are in
        for room_id in services().rooms.state_cache.rooms_joined(user_id).filter_map(|r| r.ok()) {
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
            futures.push(
                self.roomid_lasttypingupdate
                    .watch_prefix(&roomid_bytes),
            );

            futures.push(
                self.readreceiptid_readreceipt
                    .watch_prefix(&roomid_prefix),
            );

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
        futures.push(
            self.userid_lastonetimekeyupdate
                .watch_prefix(&userid_bytes),
        );

        futures.push(Box::pin(services().globals.rotate.watch()));

        // Wait until one of them finds something
        futures.next().await;

        Ok(())
    }

    fn cleanup(&self) -> Result<()> {
        self._db.cleanup()
    }

    fn memory_usage(&self) -> Result<String> {
        self._db.memory_usage()
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

        keypair
    }
    fn remove_keypair(&self) -> Result<()> {
        self.global.remove(b"keypair")
    }

    fn add_signing_key(
        &self,
        origin: &ServerName,
        new_keys: ServerSigningKeys,
    ) -> Result<BTreeMap<Box<ServerSigningKeyId>, VerifyKey>> {
        // Not atomic, but this is not critical
        let signingkeys = self.server_signingkeys.get(origin.as_bytes())?;

        let mut keys = signingkeys
            .and_then(|keys| serde_json::from_slice(&keys).ok())
            .unwrap_or_else(|| {
                // Just insert "now", it doesn't matter
                ServerSigningKeys::new(origin.to_owned(), MilliSecondsSinceUnixEpoch::now())
            });

        let ServerSigningKeys {
            verify_keys,
            old_verify_keys,
            ..
        } = new_keys;

        keys.verify_keys.extend(verify_keys.into_iter());
        keys.old_verify_keys.extend(old_verify_keys.into_iter());

        self.server_signingkeys.insert(
            origin.as_bytes(),
            &serde_json::to_vec(&keys).expect("serversigningkeys can be serialized"),
        )?;

        let mut tree = keys.verify_keys;
        tree.extend(
            keys.old_verify_keys
                .into_iter()
                .map(|old| (old.0, VerifyKey::new(old.1.key))),
        );

        Ok(tree)
    }

    /// This returns an empty `Ok(BTreeMap<..>)` when there are no keys found for the server.
    fn signing_keys_for(
        &self,
        origin: &ServerName,
    ) -> Result<BTreeMap<Box<ServerSigningKeyId>, VerifyKey>> {
        let signingkeys = self
            .server_signingkeys
            .get(origin.as_bytes())?
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .map(|keys: ServerSigningKeys| {
                let mut tree = keys.verify_keys;
                tree.extend(
                    keys.old_verify_keys
                        .into_iter()
                        .map(|old| (old.0, VerifyKey::new(old.1.key))),
                );
                tree
            })
            .unwrap_or_else(BTreeMap::new);

        Ok(signingkeys)
    }

    fn database_version(&self) -> Result<u64> {
        self.global.get(b"version")?.map_or(Ok(0), |version| {
            utils::u64_from_bytes(&version)
                .map_err(|_| Error::bad_database("Database version id is invalid."))
        })
    }

    fn bump_database_version(&self, new_version: u64) -> Result<()> {
        self.global
            .insert(b"version", &new_version.to_be_bytes())?;
        Ok(())
    }


}
