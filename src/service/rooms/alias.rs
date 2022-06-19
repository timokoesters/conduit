
    #[tracing::instrument(skip(self, globals))]
    pub fn set_alias(
        &self,
        alias: &RoomAliasId,
        room_id: Option<&RoomId>,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        if let Some(room_id) = room_id {
            // New alias
            self.alias_roomid
                .insert(alias.alias().as_bytes(), room_id.as_bytes())?;
            let mut aliasid = room_id.as_bytes().to_vec();
            aliasid.push(0xff);
            aliasid.extend_from_slice(&globals.next_count()?.to_be_bytes());
            self.aliasid_alias.insert(&aliasid, &*alias.as_bytes())?;
        } else {
            // room_id=None means remove alias
            if let Some(room_id) = self.alias_roomid.get(alias.alias().as_bytes())? {
                let mut prefix = room_id.to_vec();
                prefix.push(0xff);

                for (key, _) in self.aliasid_alias.scan_prefix(prefix) {
                    self.aliasid_alias.remove(&key)?;
                }
                self.alias_roomid.remove(alias.alias().as_bytes())?;
            } else {
                return Err(Error::BadRequest(
                    ErrorKind::NotFound,
                    "Alias does not exist.",
                ));
            }
        }

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn id_from_alias(&self, alias: &RoomAliasId) -> Result<Option<Box<RoomId>>> {
        self.alias_roomid
            .get(alias.alias().as_bytes())?
            .map(|bytes| {
                RoomId::parse(utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Room ID in alias_roomid is invalid unicode.")
                })?)
                .map_err(|_| Error::bad_database("Room ID in alias_roomid is invalid."))
            })
            .transpose()
    }

    #[tracing::instrument(skip(self))]
    pub fn room_aliases<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<Box<RoomAliasId>>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.aliasid_alias.scan_prefix(prefix).map(|(_, bytes)| {
            utils::string_from_bytes(&bytes)
                .map_err(|_| Error::bad_database("Invalid alias bytes in aliasid_alias."))?
                .try_into()
                .map_err(|_| Error::bad_database("Invalid alias in aliasid_alias."))
        })
    }

