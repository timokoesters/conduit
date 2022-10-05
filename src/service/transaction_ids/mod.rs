mod data;
use std::sync::Arc;

pub use data::Data;

use ruma::{UserId, DeviceId, TransactionId};
use crate::Result;

pub struct Service {
    db: Arc<dyn Data>,
}

impl Service {
    pub fn add_txnid(
        &self,
        user_id: &UserId,
        device_id: Option<&DeviceId>,
        txn_id: &TransactionId,
        data: &[u8],
    ) -> Result<()> {
        self.db.add_txnid(user_id, device_id, txn_id, data)
    }

    pub fn existing_txnid(
        &self,
        user_id: &UserId,
        device_id: Option<&DeviceId>,
        txn_id: &TransactionId,
    ) -> Result<Option<Vec<u8>>> {
        self.db.existing_txnid(user_id, device_id, txn_id)
    }
}
