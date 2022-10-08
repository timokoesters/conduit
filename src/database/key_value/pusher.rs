use ruma::{
    api::client::push::{get_pushers, set_pusher},
    UserId,
};

use crate::{database::KeyValueDatabase, service, Error, Result, utils};

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

    fn get_pusher(&self, sender: &UserId, pushkey: &str) -> Result<Option<get_pushers::v3::Pusher>> {
        let mut senderkey = sender.as_bytes().to_vec();
        senderkey.push(0xff);
        senderkey.extend_from_slice(pushkey.as_bytes());

        self.senderkey_pusher
            .get(&senderkey)?
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

    fn get_pushkeys<'a>(&'a self, sender: &UserId) -> Box<dyn Iterator<Item = Result<String>> + 'a> {
        let mut prefix = sender.as_bytes().to_vec();
        prefix.push(0xff);

        Box::new(self.senderkey_pusher.scan_prefix(prefix).map(|(k, _)| {
            let mut parts = k.splitn(2, |&b| b == 0xff);
            let _senderkey = parts.next();
            let push_key = parts.next().ok_or_else(|| Error::bad_database("Invalid senderkey_pusher in db"))?;
            let push_key_string = utils::string_from_bytes(push_key).map_err(|_| Error::bad_database("Invalid pusher bytes in senderkey_pusher"))?;

            Ok(push_key_string)
        }))
    }
}
