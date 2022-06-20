
    /// Returns a stack with info on shortstatehash, full state, added diff and removed diff for the selected shortstatehash and each parent layer.
    #[tracing::instrument(skip(self))]
    pub fn load_shortstatehash_info(
        &self,
        shortstatehash: u64,
    ) -> Result<
        Vec<(
            u64,                           // sstatehash
            HashSet<CompressedStateEvent>, // full state
            HashSet<CompressedStateEvent>, // added
            HashSet<CompressedStateEvent>, // removed
        )>,
    > {
        if let Some(r) = self
            .stateinfo_cache
            .lock()
            .unwrap()
            .get_mut(&shortstatehash)
        {
            return Ok(r.clone());
        }

        let value = self
            .shortstatehash_statediff
            .get(&shortstatehash.to_be_bytes())?
            .ok_or_else(|| Error::bad_database("State hash does not exist"))?;
        let parent =
            utils::u64_from_bytes(&value[0..size_of::<u64>()]).expect("bytes have right length");

        let mut add_mode = true;
        let mut added = HashSet::new();
        let mut removed = HashSet::new();

        let mut i = size_of::<u64>();
        while let Some(v) = value.get(i..i + 2 * size_of::<u64>()) {
            if add_mode && v.starts_with(&0_u64.to_be_bytes()) {
                add_mode = false;
                i += size_of::<u64>();
                continue;
            }
            if add_mode {
                added.insert(v.try_into().expect("we checked the size above"));
            } else {
                removed.insert(v.try_into().expect("we checked the size above"));
            }
            i += 2 * size_of::<u64>();
        }

        if parent != 0_u64 {
            let mut response = self.load_shortstatehash_info(parent)?;
            let mut state = response.last().unwrap().1.clone();
            state.extend(added.iter().copied());
            for r in &removed {
                state.remove(r);
            }

            response.push((shortstatehash, state, added, removed));

            Ok(response)
        } else {
            let response = vec![(shortstatehash, added.clone(), added, removed)];
            self.stateinfo_cache
                .lock()
                .unwrap()
                .insert(shortstatehash, response.clone());
            Ok(response)
        }
    }

    pub fn compress_state_event(
        &self,
        shortstatekey: u64,
        event_id: &EventId,
        globals: &super::globals::Globals,
    ) -> Result<CompressedStateEvent> {
        let mut v = shortstatekey.to_be_bytes().to_vec();
        v.extend_from_slice(
            &self
                .get_or_create_shorteventid(event_id, globals)?
                .to_be_bytes(),
        );
        Ok(v.try_into().expect("we checked the size above"))
    }

    /// Returns shortstatekey, event id
    pub fn parse_compressed_state_event(
        &self,
        compressed_event: CompressedStateEvent,
    ) -> Result<(u64, Arc<EventId>)> {
        Ok((
            utils::u64_from_bytes(&compressed_event[0..size_of::<u64>()])
                .expect("bytes have right length"),
            self.get_eventid_from_short(
                utils::u64_from_bytes(&compressed_event[size_of::<u64>()..])
                    .expect("bytes have right length"),
            )?,
        ))
    }

    /// Creates a new shortstatehash that often is just a diff to an already existing
    /// shortstatehash and therefore very efficient.
    ///
    /// There are multiple layers of diffs. The bottom layer 0 always contains the full state. Layer
    /// 1 contains diffs to states of layer 0, layer 2 diffs to layer 1 and so on. If layer n > 0
    /// grows too big, it will be combined with layer n-1 to create a new diff on layer n-1 that's
    /// based on layer n-2. If that layer is also too big, it will recursively fix above layers too.
    ///
    /// * `shortstatehash` - Shortstatehash of this state
    /// * `statediffnew` - Added to base. Each vec is shortstatekey+shorteventid
    /// * `statediffremoved` - Removed from base. Each vec is shortstatekey+shorteventid
    /// * `diff_to_sibling` - Approximately how much the diff grows each time for this layer
    /// * `parent_states` - A stack with info on shortstatehash, full state, added diff and removed diff for each parent layer
    #[tracing::instrument(skip(
        self,
        statediffnew,
        statediffremoved,
        diff_to_sibling,
        parent_states
    ))]
    pub fn save_state_from_diff(
        &self,
        shortstatehash: u64,
        statediffnew: HashSet<CompressedStateEvent>,
        statediffremoved: HashSet<CompressedStateEvent>,
        diff_to_sibling: usize,
        mut parent_states: Vec<(
            u64,                           // sstatehash
            HashSet<CompressedStateEvent>, // full state
            HashSet<CompressedStateEvent>, // added
            HashSet<CompressedStateEvent>, // removed
        )>,
    ) -> Result<()> {
        let diffsum = statediffnew.len() + statediffremoved.len();

        if parent_states.len() > 3 {
            // Number of layers
            // To many layers, we have to go deeper
            let parent = parent_states.pop().unwrap();

            let mut parent_new = parent.2;
            let mut parent_removed = parent.3;

            for removed in statediffremoved {
                if !parent_new.remove(&removed) {
                    // It was not added in the parent and we removed it
                    parent_removed.insert(removed);
                }
                // Else it was added in the parent and we removed it again. We can forget this change
            }

            for new in statediffnew {
                if !parent_removed.remove(&new) {
                    // It was not touched in the parent and we added it
                    parent_new.insert(new);
                }
                // Else it was removed in the parent and we added it again. We can forget this change
            }

            self.save_state_from_diff(
                shortstatehash,
                parent_new,
                parent_removed,
                diffsum,
                parent_states,
            )?;

            return Ok(());
        }

        if parent_states.is_empty() {
            // There is no parent layer, create a new state
            let mut value = 0_u64.to_be_bytes().to_vec(); // 0 means no parent
            for new in &statediffnew {
                value.extend_from_slice(&new[..]);
            }

            if !statediffremoved.is_empty() {
                warn!("Tried to create new state with removals");
            }

            self.shortstatehash_statediff
                .insert(&shortstatehash.to_be_bytes(), &value)?;

            return Ok(());
        };

        // Else we have two options.
        // 1. We add the current diff on top of the parent layer.
        // 2. We replace a layer above

        let parent = parent_states.pop().unwrap();
        let parent_diff = parent.2.len() + parent.3.len();

        if diffsum * diffsum >= 2 * diff_to_sibling * parent_diff {
            // Diff too big, we replace above layer(s)
            let mut parent_new = parent.2;
            let mut parent_removed = parent.3;

            for removed in statediffremoved {
                if !parent_new.remove(&removed) {
                    // It was not added in the parent and we removed it
                    parent_removed.insert(removed);
                }
                // Else it was added in the parent and we removed it again. We can forget this change
            }

            for new in statediffnew {
                if !parent_removed.remove(&new) {
                    // It was not touched in the parent and we added it
                    parent_new.insert(new);
                }
                // Else it was removed in the parent and we added it again. We can forget this change
            }

            self.save_state_from_diff(
                shortstatehash,
                parent_new,
                parent_removed,
                diffsum,
                parent_states,
            )?;
        } else {
            // Diff small enough, we add diff as layer on top of parent
            let mut value = parent.0.to_be_bytes().to_vec();
            for new in &statediffnew {
                value.extend_from_slice(&new[..]);
            }

            if !statediffremoved.is_empty() {
                value.extend_from_slice(&0_u64.to_be_bytes());
                for removed in &statediffremoved {
                    value.extend_from_slice(&removed[..]);
                }
            }

            self.shortstatehash_statediff
                .insert(&shortstatehash.to_be_bytes(), &value)?;
        }

        Ok(())
    }

    /// Returns the new shortstatehash
    pub fn save_state(
        room_id: &RoomId,
        new_state_ids_compressed: HashSet<CompressedStateEvent>,
    ) -> Result<(u64, 
            HashSet<CompressedStateEvent>, // added
            HashSet<CompressedStateEvent>)> // removed
    {
        let previous_shortstatehash = self.d.current_shortstatehash(room_id)?;

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

        Ok((new_shortstatehash, statediffnew, statediffremoved))
    }

    #[tracing::instrument(skip(self))]
    pub fn get_auth_chain_from_cache<'a>(
        &'a self,
        key: &[u64],
    ) -> Result<Option<Arc<HashSet<u64>>>> {
        // Check RAM cache
        if let Some(result) = self.auth_chain_cache.lock().unwrap().get_mut(key) {
            return Ok(Some(Arc::clone(result)));
        }

        // Check DB cache
        if key.len() == 1 {
            if let Some(chain) =
                self.shorteventid_authchain
                    .get(&key[0].to_be_bytes())?
                    .map(|chain| {
                        chain
                            .chunks_exact(size_of::<u64>())
                            .map(|chunk| {
                                utils::u64_from_bytes(chunk).expect("byte length is correct")
                            })
                            .collect()
                    })
            {
                let chain = Arc::new(chain);

                // Cache in RAM
                self.auth_chain_cache
                    .lock()
                    .unwrap()
                    .insert(vec![key[0]], Arc::clone(&chain));

                return Ok(Some(chain));
            }
        }

        Ok(None)
    }

    #[tracing::instrument(skip(self))]
    pub fn cache_auth_chain(&self, key: Vec<u64>, chain: Arc<HashSet<u64>>) -> Result<()> {
        // Persist in db
        if key.len() == 1 {
            self.shorteventid_authchain.insert(
                &key[0].to_be_bytes(),
                &chain
                    .iter()
                    .flat_map(|s| s.to_be_bytes().to_vec())
                    .collect::<Vec<u8>>(),
            )?;
        }

        // Cache in RAM
        self.auth_chain_cache.lock().unwrap().insert(key, chain);

        Ok(())
    }
