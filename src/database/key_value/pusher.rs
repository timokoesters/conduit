use ruma::{UserId, api::client::push::{set_pusher, get_pushers}};

use crate::{service, database::KeyValueDatabase, Error};

impl service::pusher::Data for KeyValueDatabase {
    fn set_pusher(&self, sender: &UserId, pusher: set_pusher::v3::Pusher) -> Result<()> {
        let mut key = sender.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(pusher.pushkey.as_bytes());

        // There are 2 kinds of pushers but the spec says: null deletes the pusher.
        if pusher.kind.is_none() {
            return self
                .senderkey_pusher
                .remove(&key)
                .map(|_| ())
                .map_err(Into::into);
        }

        self.senderkey_pusher.insert(
            &key,
            &serde_json::to_vec(&pusher).expect("Pusher is valid JSON value"),
        )?;

        Ok(())
    }

    fn get_pusher(&self, senderkey: &[u8]) -> Result<Option<get_pushers::v3::Pusher>> {
        self.senderkey_pusher
            .get(senderkey)?
            .map(|push| {
                serde_json::from_slice(&*push)
                    .map_err(|_| Error::bad_database("Invalid Pusher in db."))
            })
            .transpose()
    }

    fn get_pushers(&self, sender: &UserId) -> Result<Vec<get_pushers::v3::Pusher>> {
        let mut prefix = sender.as_bytes().to_vec();
        prefix.push(0xff);

        self.senderkey_pusher
            .scan_prefix(prefix)
            .map(|(_, push)| {
                serde_json::from_slice(&*push)
                    .map_err(|_| Error::bad_database("Invalid Pusher in db."))
            })
            .collect()
    }

    fn get_pusher_senderkeys<'a>(
        &'a self,
        sender: &UserId,
    ) -> impl Iterator<Item = Vec<u8>> + 'a {
        let mut prefix = sender.as_bytes().to_vec();
        prefix.push(0xff);

        self.senderkey_pusher.scan_prefix(prefix).map(|(k, _)| k)
    }
}
