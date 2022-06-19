
    /// Builds a StateMap by iterating over all keys that start
    /// with state_hash, this gives the full state for the given state_hash.
    #[tracing::instrument(skip(self))]
    pub async fn state_full_ids(&self, shortstatehash: u64) -> Result<BTreeMap<u64, Arc<EventId>>> {
        let full_state = self
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;
        let mut result = BTreeMap::new();
        let mut i = 0;
        for compressed in full_state.into_iter() {
            let parsed = self.parse_compressed_state_event(compressed)?;
            result.insert(parsed.0, parsed.1);

            i += 1;
            if i % 100 == 0 {
                tokio::task::yield_now().await;
            }
        }
        Ok(result)
    }

    #[tracing::instrument(skip(self))]
    pub async fn state_full(
        &self,
        shortstatehash: u64,
    ) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>> {
        let full_state = self
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;

        let mut result = HashMap::new();
        let mut i = 0;
        for compressed in full_state {
            let (_, eventid) = self.parse_compressed_state_event(compressed)?;
            if let Some(pdu) = self.get_pdu(&eventid)? {
                result.insert(
                    (
                        pdu.kind.to_string().into(),
                        pdu.state_key
                            .as_ref()
                            .ok_or_else(|| Error::bad_database("State event has no state key."))?
                            .clone(),
                    ),
                    pdu,
                );
            }

            i += 1;
            if i % 100 == 0 {
                tokio::task::yield_now().await;
            }
        }

        Ok(result)
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn state_get_id(
        &self,
        shortstatehash: u64,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<EventId>>> {
        let shortstatekey = match self.get_shortstatekey(event_type, state_key)? {
            Some(s) => s,
            None => return Ok(None),
        };
        let full_state = self
            .load_shortstatehash_info(shortstatehash)?
            .pop()
            .expect("there is always one layer")
            .1;
        Ok(full_state
            .into_iter()
            .find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
            .and_then(|compressed| {
                self.parse_compressed_state_event(compressed)
                    .ok()
                    .map(|(_, id)| id)
            }))
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn state_get(
        &self,
        shortstatehash: u64,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<PduEvent>>> {
        self.state_get_id(shortstatehash, event_type, state_key)?
            .map_or(Ok(None), |event_id| self.get_pdu(&event_id))
    }

    /// Returns the state hash for this pdu.
    pub fn pdu_shortstatehash(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_shorteventid
            .get(event_id.as_bytes())?
            .map_or(Ok(None), |shorteventid| {
                self.shorteventid_shortstatehash
                    .get(&shorteventid)?
                    .map(|bytes| {
                        utils::u64_from_bytes(&bytes).map_err(|_| {
                            Error::bad_database(
                                "Invalid shortstatehash bytes in shorteventid_shortstatehash",
                            )
                        })
                    })
                    .transpose()
            })
    }

    /// Returns the last state hash key added to the db for the given room.
    #[tracing::instrument(skip(self))]
    pub fn current_shortstatehash(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.roomid_shortstatehash
            .get(room_id.as_bytes())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid shortstatehash in roomid_shortstatehash")
                })?))
            })
    }

    /// Force the creation of a new StateHash and insert it into the db.
    ///
    /// Whatever `state` is supplied to `force_state` becomes the new current room state snapshot.
    #[tracing::instrument(skip(self, new_state_ids_compressed, db))]
    pub fn force_state(
        &self,
        room_id: &RoomId,
        new_state_ids_compressed: HashSet<CompressedStateEvent>,
        db: &Database,
    ) -> Result<()> {
        let previous_shortstatehash = self.current_shortstatehash(room_id)?;

        let state_hash = self.calculate_hash(
            &new_state_ids_compressed
                .iter()
                .map(|bytes| &bytes[..])
                .collect::<Vec<_>>(),
        );

        let (new_shortstatehash, already_existed) =
            self.get_or_create_shortstatehash(&state_hash, &db.globals)?;

        if Some(new_shortstatehash) == previous_shortstatehash {
            return Ok(());
        }

        let states_parents = previous_shortstatehash
            .map_or_else(|| Ok(Vec::new()), |p| self.load_shortstatehash_info(p))?;

        let (statediffnew, statediffremoved) = if let Some(parent_stateinfo) = states_parents.last()
        {
            let statediffnew: HashSet<_> = new_state_ids_compressed
                .difference(&parent_stateinfo.1)
                .copied()
                .collect();

            let statediffremoved: HashSet<_> = parent_stateinfo
                .1
                .difference(&new_state_ids_compressed)
                .copied()
                .collect();

            (statediffnew, statediffremoved)
        } else {
            (new_state_ids_compressed, HashSet::new())
        };

        if !already_existed {
            self.save_state_from_diff(
                new_shortstatehash,
                statediffnew.clone(),
                statediffremoved,
                2, // every state change is 2 event changes on average
                states_parents,
            )?;
        };

        for event_id in statediffnew.into_iter().filter_map(|new| {
            self.parse_compressed_state_event(new)
                .ok()
                .map(|(_, id)| id)
        }) {
            let pdu = match self.get_pdu_json(&event_id)? {
                Some(pdu) => pdu,
                None => continue,
            };

            if pdu.get("type").and_then(|val| val.as_str()) != Some("m.room.member") {
                continue;
            }

            let pdu: PduEvent = match serde_json::from_str(
                &serde_json::to_string(&pdu).expect("CanonicalJsonObj can be serialized to JSON"),
            ) {
                Ok(pdu) => pdu,
                Err(_) => continue,
            };

            #[derive(Deserialize)]
            struct ExtractMembership {
                membership: MembershipState,
            }

            let membership = match serde_json::from_str::<ExtractMembership>(pdu.content.get()) {
                Ok(e) => e.membership,
                Err(_) => continue,
            };

            let state_key = match pdu.state_key {
                Some(k) => k,
                None => continue,
            };

            let user_id = match UserId::parse(state_key) {
                Ok(id) => id,
                Err(_) => continue,
            };

            self.update_membership(room_id, &user_id, membership, &pdu.sender, None, db, false)?;
        }

        self.update_joined_count(room_id, db)?;

        self.roomid_shortstatehash
            .insert(room_id.as_bytes(), &new_shortstatehash.to_be_bytes())?;

        Ok(())
    }

    /// Returns the full room state.
    #[tracing::instrument(skip(self))]
    pub async fn room_state_full(
        &self,
        room_id: &RoomId,
    ) -> Result<HashMap<(StateEventType, String), Arc<PduEvent>>> {
        if let Some(current_shortstatehash) = self.current_shortstatehash(room_id)? {
            self.state_full(current_shortstatehash).await
        } else {
            Ok(HashMap::new())
        }
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn room_state_get_id(
        &self,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<EventId>>> {
        if let Some(current_shortstatehash) = self.current_shortstatehash(room_id)? {
            self.state_get_id(current_shortstatehash, event_type, state_key)
        } else {
            Ok(None)
        }
    }

    /// Returns a single PDU from `room_id` with key (`event_type`, `state_key`).
    #[tracing::instrument(skip(self))]
    pub fn room_state_get(
        &self,
        room_id: &RoomId,
        event_type: &StateEventType,
        state_key: &str,
    ) -> Result<Option<Arc<PduEvent>>> {
        if let Some(current_shortstatehash) = self.current_shortstatehash(room_id)? {
            self.state_get(current_shortstatehash, event_type, state_key)
        } else {
            Ok(None)
        }
    }

    /// Returns the leaf pdus of a room.
    #[tracing::instrument(skip(self))]
    pub fn get_pdu_leaves(&self, room_id: &RoomId) -> Result<HashSet<Arc<EventId>>> {
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

    /// Replace the leaves of a room.
    ///
    /// The provided `event_ids` become the new leaves, this allows a room to have multiple
    /// `prev_events`.
    #[tracing::instrument(skip(self))]
    pub fn replace_pdu_leaves<'a>(
        &self,
        room_id: &RoomId,
        event_ids: impl IntoIterator<Item = &'a EventId> + Debug,
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

    /// Generates a new StateHash and associates it with the incoming event.
    ///
    /// This adds all current state events (not including the incoming event)
    /// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
    #[tracing::instrument(skip(self, state_ids_compressed, globals))]
    pub fn set_event_state(
        &self,
        event_id: &EventId,
        room_id: &RoomId,
        state_ids_compressed: HashSet<CompressedStateEvent>,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let shorteventid = self.get_or_create_shorteventid(event_id, globals)?;

        let previous_shortstatehash = self.current_shortstatehash(room_id)?;

        let state_hash = self.calculate_hash(
            &state_ids_compressed
                .iter()
                .map(|s| &s[..])
                .collect::<Vec<_>>(),
        );

        let (shortstatehash, already_existed) =
            self.get_or_create_shortstatehash(&state_hash, globals)?;

        if !already_existed {
            let states_parents = previous_shortstatehash
                .map_or_else(|| Ok(Vec::new()), |p| self.load_shortstatehash_info(p))?;

            let (statediffnew, statediffremoved) =
                if let Some(parent_stateinfo) = states_parents.last() {
                    let statediffnew: HashSet<_> = state_ids_compressed
                        .difference(&parent_stateinfo.1)
                        .copied()
                        .collect();

                    let statediffremoved: HashSet<_> = parent_stateinfo
                        .1
                        .difference(&state_ids_compressed)
                        .copied()
                        .collect();

                    (statediffnew, statediffremoved)
                } else {
                    (state_ids_compressed, HashSet::new())
                };
            self.save_state_from_diff(
                shortstatehash,
                statediffnew,
                statediffremoved,
                1_000_000, // high number because no state will be based on this one
                states_parents,
            )?;
        }

        self.shorteventid_shortstatehash
            .insert(&shorteventid.to_be_bytes(), &shortstatehash.to_be_bytes())?;

        Ok(())
    }

    /// Generates a new StateHash and associates it with the incoming event.
    ///
    /// This adds all current state events (not including the incoming event)
    /// to `stateid_pduid` and adds the incoming event to `eventid_statehash`.
    #[tracing::instrument(skip(self, new_pdu, globals))]
    pub fn append_to_state(
        &self,
        new_pdu: &PduEvent,
        globals: &super::globals::Globals,
    ) -> Result<u64> {
        let shorteventid = self.get_or_create_shorteventid(&new_pdu.event_id, globals)?;

        let previous_shortstatehash = self.current_shortstatehash(&new_pdu.room_id)?;

        if let Some(p) = previous_shortstatehash {
            self.shorteventid_shortstatehash
                .insert(&shorteventid.to_be_bytes(), &p.to_be_bytes())?;
        }

        if let Some(state_key) = &new_pdu.state_key {
            let states_parents = previous_shortstatehash
                .map_or_else(|| Ok(Vec::new()), |p| self.load_shortstatehash_info(p))?;

            let shortstatekey = self.get_or_create_shortstatekey(
                &new_pdu.kind.to_string().into(),
                state_key,
                globals,
            )?;

            let new = self.compress_state_event(shortstatekey, &new_pdu.event_id, globals)?;

            let replaces = states_parents
                .last()
                .map(|info| {
                    info.1
                        .iter()
                        .find(|bytes| bytes.starts_with(&shortstatekey.to_be_bytes()))
                })
                .unwrap_or_default();

            if Some(&new) == replaces {
                return Ok(previous_shortstatehash.expect("must exist"));
            }

            // TODO: statehash with deterministic inputs
            let shortstatehash = globals.next_count()?;

            let mut statediffnew = HashSet::new();
            statediffnew.insert(new);

            let mut statediffremoved = HashSet::new();
            if let Some(replaces) = replaces {
                statediffremoved.insert(*replaces);
            }

            self.save_state_from_diff(
                shortstatehash,
                statediffnew,
                statediffremoved,
                2,
                states_parents,
            )?;

            Ok(shortstatehash)
        } else {
            Ok(previous_shortstatehash.expect("first event in room must be a state event"))
        }
    }

    #[tracing::instrument(skip(self, invite_event))]
    pub fn calculate_invite_state(
        &self,
        invite_event: &PduEvent,
    ) -> Result<Vec<Raw<AnyStrippedStateEvent>>> {
        let mut state = Vec::new();
        // Add recommended events
        if let Some(e) =
            self.room_state_get(&invite_event.room_id, &StateEventType::RoomCreate, "")?
        {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) =
            self.room_state_get(&invite_event.room_id, &StateEventType::RoomJoinRules, "")?
        {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) = self.room_state_get(
            &invite_event.room_id,
            &StateEventType::RoomCanonicalAlias,
            "",
        )? {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) =
            self.room_state_get(&invite_event.room_id, &StateEventType::RoomAvatar, "")?
        {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) =
            self.room_state_get(&invite_event.room_id, &StateEventType::RoomName, "")?
        {
            state.push(e.to_stripped_state_event());
        }
        if let Some(e) = self.room_state_get(
            &invite_event.room_id,
            &StateEventType::RoomMember,
            invite_event.sender.as_str(),
        )? {
            state.push(e.to_stripped_state_event());
        }

        state.push(invite_event.to_stripped_state_event());
        Ok(state)
    }

    #[tracing::instrument(skip(self))]
    pub fn set_room_state(&self, room_id: &RoomId, shortstatehash: u64) -> Result<()> {
        self.roomid_shortstatehash
            .insert(room_id.as_bytes(), &shortstatehash.to_be_bytes())?;

        Ok(())
    }
