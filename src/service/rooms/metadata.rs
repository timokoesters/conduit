    /// Checks if a room exists.
    #[tracing::instrument(skip(self))]
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        let prefix = match self.get_shortroomid(room_id)? {
            Some(b) => b.to_be_bytes().to_vec(),
            None => return Ok(false),
        };

        // Look for PDUs in that room.
        Ok(self
            .pduid_pdu
            .iter_from(&prefix, false)
            .next()
            .filter(|(k, _)| k.starts_with(&prefix))
            .is_some())
    }

    pub fn get_shortroomid(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.roomid_shortroomid
            .get(room_id.as_bytes())?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid shortroomid in db."))
            })
            .transpose()
    }

    pub fn get_or_create_shortroomid(
        &self,
        room_id: &RoomId,
        globals: &super::globals::Globals,
    ) -> Result<u64> {
        Ok(match self.roomid_shortroomid.get(room_id.as_bytes())? {
            Some(short) => utils::u64_from_bytes(&short)
                .map_err(|_| Error::bad_database("Invalid shortroomid in db."))?,
            None => {
                let short = globals.next_count()?;
                self.roomid_shortroomid
                    .insert(room_id.as_bytes(), &short.to_be_bytes())?;
                short
            }
        })
    }

