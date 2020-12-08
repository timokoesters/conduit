use crate::Result;
use ruma::{DeviceId, UserId};
use sled::IVec;

#[derive(Clone)]
pub struct TransactionIds {
    pub(super) userdevicetxnid_response: sled::Tree, // Response can be empty (/sendToDevice) or the event id (/send)
}

impl TransactionIds {
    pub fn add_txnid(
        &self,
        user_id: &UserId,
        device_id: Option<&DeviceId>,
        txn_id: &str,
        data: &[u8],
    ) -> Result<()> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.map(|d| d.as_bytes()).unwrap_or_default());
        key.push(0xff);
        key.extend_from_slice(txn_id.as_bytes());

        self.userdevicetxnid_response.insert(key, data)?;

        Ok(())
    }

    pub fn existing_txnid(
        &self,
        user_id: &UserId,
        device_id: Option<&DeviceId>,
        txn_id: &str,
    ) -> Result<Option<IVec>> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.map(|d| d.as_bytes()).unwrap_or_default());
        key.push(0xff);
        key.extend_from_slice(txn_id.as_bytes());

        // If there's no entry, this is a new transaction
        Ok(self.userdevicetxnid_response.get(key)?)
    }
}
