use crate::service;

impl service::room::state::Data for KeyValueDatabase {
    fn get_room_shortstatehash(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.roomid_shortstatehash
            .get(room_id.as_bytes())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid shortstatehash in roomid_shortstatehash")
                })?))
            })
    }

    fn set_room_state(&self, room_id: &RoomId, new_shortstatehash: u64
        _mutex_lock: &MutexGuard<'_, StateLock>, // Take mutex guard to make sure users get the room state mutex
        ) -> Result<()> {
        self.roomid_shortstatehash
            .insert(room_id.as_bytes(), &new_shortstatehash.to_be_bytes())?;
        Ok(())
    }

    fn set_event_state(&self) -> Result<()> {
        db.shorteventid_shortstatehash
            .insert(&shorteventid.to_be_bytes(), &shortstatehash.to_be_bytes())?;
        Ok(())
    }

    fn get_pdu_leaves(&self, room_id: &RoomId) -> Result<HashSet<Arc<EventId>>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomid_pduleaves
            .scan_prefix(prefix)
            .map(|(_, bytes)| {
                EventId::parse_arc(utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("EventID in roomid_pduleaves is invalid unicode.")
                })?)
                .map_err(|_| Error::bad_database("EventId in roomid_pduleaves is invalid."))
            })
            .collect()
    }

    fn set_forward_extremities(
        &self,
        room_id: &RoomId,
        event_ids: impl IntoIterator<Item = &'a EventId> + Debug,
        _mutex_lock: &MutexGuard<'_, StateLock>, // Take mutex guard to make sure users get the room state mutex
    ) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        for (key, _) in self.roomid_pduleaves.scan_prefix(prefix.clone()) {
            self.roomid_pduleaves.remove(&key)?;
        }

        for event_id in event_ids {
            let mut key = prefix.to_owned();
            key.extend_from_slice(event_id.as_bytes());
            self.roomid_pduleaves.insert(&key, event_id.as_bytes())?;
        }

        Ok(())
    }

}

impl service::room::alias::Data for KeyValueDatabase {
    fn set_alias(
        &self,
        alias: &RoomAliasId,
        room_id: Option<&RoomId>
    ) -> Result<()> {
        self.alias_roomid
            .insert(alias.alias().as_bytes(), room_id.as_bytes())?;
        let mut aliasid = room_id.as_bytes().to_vec();
        aliasid.push(0xff);
        aliasid.extend_from_slice(&globals.next_count()?.to_be_bytes());
        self.aliasid_alias.insert(&aliasid, &*alias.as_bytes())?;
        Ok(())
    }

    fn remove_alias(
        &self,
        alias: &RoomAliasId,
    ) -> Result<()> {
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
        Ok(())
    }

    fn resolve_local_alias(
        &self,
        alias: &RoomAliasId
    ) -> Result<()> {
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

    fn local_aliases_for_room(
        &self,
        room_id: &RoomId,
    ) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.aliasid_alias.scan_prefix(prefix).map(|(_, bytes)| {
            utils::string_from_bytes(&bytes)
                .map_err(|_| Error::bad_database("Invalid alias bytes in aliasid_alias."))?
                .try_into()
                .map_err(|_| Error::bad_database("Invalid alias in aliasid_alias."))
        })
    }
}

impl service::room::directory::Data for KeyValueDatabase {
    fn set_public(&self, room_id: &RoomId) -> Result<()> {
        self.publicroomids.insert(room_id.as_bytes(), &[])?;
    }

    fn set_not_public(&self, room_id: &RoomId) -> Result<()> {
        self.publicroomids.remove(room_id.as_bytes())?;
    }

    fn is_public_room(&self, room_id: &RoomId) -> Result<bool> {
        Ok(self.publicroomids.get(room_id.as_bytes())?.is_some())
    }

    fn public_rooms(&self) -> impl Iterator<Item = Result<Box<RoomId>>> + '_ {
        self.publicroomids.iter().map(|(bytes, _)| {
            RoomId::parse(
                utils::string_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Room ID in publicroomids is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Room ID in publicroomids is invalid."))
        })
    }
}

impl service::room::edus::read_receipt::Data for KeyValueDatabase {
    fn readreceipt_update(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        event: ReceiptEvent,
    ) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut last_possible_key = prefix.clone();
        last_possible_key.extend_from_slice(&u64::MAX.to_be_bytes());

        // Remove old entry
        if let Some((old, _)) = self
            .readreceiptid_readreceipt
            .iter_from(&last_possible_key, true)
            .take_while(|(key, _)| key.starts_with(&prefix))
            .find(|(key, _)| {
                key.rsplit(|&b| b == 0xff)
                    .next()
                    .expect("rsplit always returns an element")
                    == user_id.as_bytes()
            })
        {
            // This is the old room_latest
            self.readreceiptid_readreceipt.remove(&old)?;
        }

        let mut room_latest_id = prefix;
        room_latest_id.extend_from_slice(&globals.next_count()?.to_be_bytes());
        room_latest_id.push(0xff);
        room_latest_id.extend_from_slice(user_id.as_bytes());

        self.readreceiptid_readreceipt.insert(
            &room_latest_id,
            &serde_json::to_vec(&event).expect("EduEvent::to_string always works"),
        )?;

        Ok(())
    }

    pub fn readreceipts_since<'a>(
        &'a self,
        room_id: &RoomId,
        since: u64,
    ) -> impl Iterator<
        Item=Result<(
            Box<UserId>,
            u64,
            Raw<ruma::events::AnySyncEphemeralRoomEvent>,
        )>,
    > + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);
        let prefix2 = prefix.clone();

        let mut first_possible_edu = prefix.clone();
        first_possible_edu.extend_from_slice(&(since + 1).to_be_bytes()); // +1 so we don't send the event at since

        self.readreceiptid_readreceipt
            .iter_from(&first_possible_edu, false)
            .take_while(move |(k, _)| k.starts_with(&prefix2))
            .map(move |(k, v)| {
                let count =
                    utils::u64_from_bytes(&k[prefix.len()..prefix.len() + mem::size_of::<u64>()])
                        .map_err(|_| Error::bad_database("Invalid readreceiptid count in db."))?;
                let user_id = UserId::parse(
                    utils::string_from_bytes(&k[prefix.len() + mem::size_of::<u64>() + 1..])
                        .map_err(|_| {
                            Error::bad_database("Invalid readreceiptid userid bytes in db.")
                        })?,
                )
                    .map_err(|_| Error::bad_database("Invalid readreceiptid userid in db."))?;

                let mut json = serde_json::from_slice::<CanonicalJsonObject>(&v).map_err(|_| {
                    Error::bad_database("Read receipt in roomlatestid_roomlatest is invalid json.")
                })?;
                json.remove("room_id");

                Ok((
                    user_id,
                    count,
                    Raw::from_json(
                        serde_json::value::to_raw_value(&json).expect("json is valid raw value"),
                    ),
                ))
            })
    }

    fn private_read_set(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        count: u64,
    ) -> Result<()> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_privateread
            .insert(&key, &count.to_be_bytes())?;

        self.roomuserid_lastprivatereadupdate
            .insert(&key, &globals.next_count()?.to_be_bytes())?;
    }

    fn private_read_get(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_privateread
            .get(&key)?
            .map_or(Ok(None), |v| {
                Ok(Some(utils::u64_from_bytes(&v).map_err(|_| {
                    Error::bad_database("Invalid private read marker bytes")
                })?))
            })
    }

    fn last_privateread_update(&self, user_id: &UserId, room_id: &RoomId) -> Result<u64> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        Ok(self
            .roomuserid_lastprivatereadupdate
            .get(&key)?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Count in roomuserid_lastprivatereadupdate is invalid.")
                })
            })
            .transpose()?
            .unwrap_or(0))
    }
}

impl service::room::edus::typing::Data for KeyValueDatabase {
    fn typing_add(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        timeout: u64,
        globals: &super::super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let count = globals.next_count()?.to_be_bytes();

        let mut room_typing_id = prefix;
        room_typing_id.extend_from_slice(&timeout.to_be_bytes());
        room_typing_id.push(0xff);
        room_typing_id.extend_from_slice(&count);

        self.typingid_userid
            .insert(&room_typing_id, &*user_id.as_bytes())?;

        self.roomid_lasttypingupdate
            .insert(room_id.as_bytes(), &count)?;

        Ok(())
    }

    fn typing_remove(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<()> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let user_id = user_id.to_string();

        let mut found_outdated = false;

        // Maybe there are multiple ones from calling roomtyping_add multiple times
        for outdated_edu in self
            .typingid_userid
            .scan_prefix(prefix)
            .filter(|(_, v)| &**v == user_id.as_bytes())
        {
            self.typingid_userid.remove(&outdated_edu.0)?;
            found_outdated = true;
        }

        if found_outdated {
            self.roomid_lasttypingupdate
                .insert(room_id.as_bytes(), &globals.next_count()?.to_be_bytes())?;
        }

        Ok(())
    }

    fn last_typing_update(
        &self,
        room_id: &RoomId,
    ) -> Result<u64> {
        Ok(self
            .roomid_lasttypingupdate
            .get(room_id.as_bytes())?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Count in roomid_lastroomactiveupdate is invalid.")
                })
            })
            .transpose()?
            .unwrap_or(0))
    }

    fn typings_all(
        &self,
        room_id: &RoomId,
    ) -> Result<HashSet<UserId>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut user_ids = HashSet::new();

        for (_, user_id) in self.typingid_userid.scan_prefix(prefix) {
            let user_id = UserId::parse(utils::string_from_bytes(&user_id).map_err(|_| {
                Error::bad_database("User ID in typingid_userid is invalid unicode.")
            })?)
                .map_err(|_| Error::bad_database("User ID in typingid_userid is invalid."))?;

            user_ids.insert(user_id);
        }

        Ok(user_ids)
    }
}

impl service::room::edus::presence::Data for KeyValueDatabase {
    fn update_presence(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        presence: PresenceEvent,
    ) -> Result<()> {
        // TODO: Remove old entry? Or maybe just wipe completely from time to time?

        let count = globals.next_count()?.to_be_bytes();

        let mut presence_id = room_id.as_bytes().to_vec();
        presence_id.push(0xff);
        presence_id.extend_from_slice(&count);
        presence_id.push(0xff);
        presence_id.extend_from_slice(presence.sender.as_bytes());

        self.presenceid_presence.insert(
            &presence_id,
            &serde_json::to_vec(&presence).expect("PresenceEvent can be serialized"),
        )?;

        self.userid_lastpresenceupdate.insert(
            user_id.as_bytes(),
            &utils::millis_since_unix_epoch().to_be_bytes(),
        )?;

        Ok(())
    }

    fn ping_presence(&self, user_id: &UserId) -> Result<()> {
        self.userid_lastpresenceupdate.insert(
            user_id.as_bytes(),
            &utils::millis_since_unix_epoch().to_be_bytes(),
        )?;

        Ok(())
    }

    fn last_presence_update(&self, user_id: &UserId) -> Result<Option<u64>> {
        self.userid_lastpresenceupdate
            .get(user_id.as_bytes())?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid timestamp in userid_lastpresenceupdate.")
                })
            })
            .transpose()
    }

    fn get_presence_event(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        count: u64,
    ) -> Result<Option<PresenceEvent>> {
        let mut presence_id = room_id.as_bytes().to_vec();
        presence_id.push(0xff);
        presence_id.extend_from_slice(&count.to_be_bytes());
        presence_id.push(0xff);
        presence_id.extend_from_slice(user_id.as_bytes());

        self.presenceid_presence
            .get(&presence_id)?
            .map(|value| parse_presence_event(&value))
            .transpose()
    }

    fn presence_since(
        &self,
        room_id: &RoomId,
        since: u64,
    ) -> Result<HashMap<Box<UserId>, PresenceEvent>> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        let mut first_possible_edu = prefix.clone();
        first_possible_edu.extend_from_slice(&(since + 1).to_be_bytes()); // +1 so we don't send the event at since
        let mut hashmap = HashMap::new();

        for (key, value) in self
            .presenceid_presence
            .iter_from(&*first_possible_edu, false)
            .take_while(|(key, _)| key.starts_with(&prefix))
        {
            let user_id = UserId::parse(
                utils::string_from_bytes(
                    key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| Error::bad_database("Invalid UserId bytes in presenceid_presence."))?,
            )
            .map_err(|_| Error::bad_database("Invalid UserId in presenceid_presence."))?;

            let presence = parse_presence_event(&value)?;

            hashmap.insert(user_id, presence);
        }

        Ok(hashmap)
    }
}

fn parse_presence_event(bytes: &[u8]) -> Result<PresenceEvent> {
    let mut presence: PresenceEvent = serde_json::from_slice(bytes)
        .map_err(|_| Error::bad_database("Invalid presence event in db."))?;

    let current_timestamp: UInt = utils::millis_since_unix_epoch()
        .try_into()
        .expect("time is valid");

    if presence.content.presence == PresenceState::Online {
        // Don't set last_active_ago when the user is online
        presence.content.last_active_ago = None;
    } else {
        // Convert from timestamp to duration
        presence.content.last_active_ago = presence
            .content
            .last_active_ago
            .map(|timestamp| current_timestamp - timestamp);
    }
}

impl service::room::lazy_load::Data for KeyValueDatabase {
    fn lazy_load_was_sent_before(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        ll_user: &UserId,
    ) -> Result<bool> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(device_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(room_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(ll_user.as_bytes());
        Ok(self.lazyloadedids.get(&key)?.is_some())
    }

    fn lazy_load_confirm_delivery(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<()> {
        if let Some(user_ids) = self.lazy_load_waiting.lock().unwrap().remove(&(
            user_id.to_owned(),
            device_id.to_owned(),
            room_id.to_owned(),
            since,
        )) {
            let mut prefix = user_id.as_bytes().to_vec();
            prefix.push(0xff);
            prefix.extend_from_slice(device_id.as_bytes());
            prefix.push(0xff);
            prefix.extend_from_slice(room_id.as_bytes());
            prefix.push(0xff);

            for ll_id in user_ids {
                let mut key = prefix.clone();
                key.extend_from_slice(ll_id.as_bytes());
                self.lazyloadedids.insert(&key, &[])?;
            }
        }

        Ok(())
    }

    fn lazy_load_reset(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        room_id: &RoomId,
    ) -> Result<()> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(device_id.as_bytes());
        prefix.push(0xff);
        prefix.extend_from_slice(room_id.as_bytes());
        prefix.push(0xff);

        for (key, _) in self.lazyloadedids.scan_prefix(prefix) {
            self.lazyloadedids.remove(&key)?;
        }

        Ok(())
    }
}

impl service::room::metadata::Data for KeyValueDatabase {
    fn exists(&self, room_id: &RoomId) -> Result<bool> {
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
}

impl service::room::outlier::Data for KeyValueDatabase {
    fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>> {
        self.eventid_outlierpdu
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu| {
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
    }

    fn get_outlier_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.eventid_outlierpdu
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |pdu| {
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
    }

    fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) -> Result<()> {
        self.eventid_outlierpdu.insert(
            event_id.as_bytes(),
            &serde_json::to_vec(&pdu).expect("CanonicalJsonObject is valid"),
        )
    }
}

impl service::room::pdu_metadata::Data for KeyValueDatabase {
    fn mark_as_referenced(&self, room_id: &RoomId, event_ids: &[Arc<EventId>]) -> Result<()> {
        for prev in event_ids {
            let mut key = room_id.as_bytes().to_vec();
            key.extend_from_slice(prev.as_bytes());
            self.referencedevents.insert(&key, &[])?;
        }

        Ok(())
    }

    fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> Result<bool> {
        let mut key = room_id.as_bytes().to_vec();
        key.extend_from_slice(event_id.as_bytes());
        Ok(self.referencedevents.get(&key)?.is_some())
    }

    fn mark_event_soft_failed(&self, event_id: &EventId) -> Result<()> {
        self.softfailedeventids.insert(event_id.as_bytes(), &[])
    }

    fn is_event_soft_failed(&self, event_id: &EventId) -> Result<bool> {
        self.softfailedeventids
            .get(event_id.as_bytes())
            .map(|o| o.is_some())
    }
}
