impl service::pusher::Data for KeyValueDatabase {
    pub fn add_txnid(
        &self,
        user_id: &UserId,
        device_id: Option<&DeviceId>,
        txn_id: &TransactionId,
        data: &[u8],
    ) -> Result<()> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.map(|d| d.as_bytes()).unwrap_or_default());
        key.push(0xff);
        key.extend_from_slice(txn_id.as_bytes());

        self.userdevicetxnid_response.insert(&key, data)?;

        Ok(())
    }

    pub fn existing_txnid(
        &self,
        user_id: &UserId,
        device_id: Option<&DeviceId>,
        txn_id: &TransactionId,
    ) -> Result<Option<Vec<u8>>> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.map(|d| d.as_bytes()).unwrap_or_default());
        key.push(0xff);
        key.extend_from_slice(txn_id.as_bytes());

        // If there's no entry, this is a new transaction
        self.userdevicetxnid_response.get(&key)
    }
}
