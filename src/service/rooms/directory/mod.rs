
    #[tracing::instrument(skip(self))]
    pub fn set_public(&self, room_id: &RoomId, public: bool) -> Result<()> {
        if public {
            self.publicroomids.insert(room_id.as_bytes(), &[])?;
        } else {
            self.publicroomids.remove(room_id.as_bytes())?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn is_public_room(&self, room_id: &RoomId) -> Result<bool> {
        Ok(self.publicroomids.get(room_id.as_bytes())?.is_some())
    }

    #[tracing::instrument(skip(self))]
    pub fn public_rooms(&self) -> impl Iterator<Item = Result<Box<RoomId>>> + '_ {
        self.publicroomids.iter().map(|(bytes, _)| {
            RoomId::parse(
                utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Room ID in publicroomids is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Room ID in publicroomids is invalid."))
        })
    }

