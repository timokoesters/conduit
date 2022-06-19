
    #[tracing::instrument(skip(self))]
    pub fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<Option<(impl Iterator<Item = Vec<u8>> + 'a, Vec<String>)>> {
        let prefix = self
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();
        let prefix_clone = prefix.clone();

        let words: Vec<_> = search_string
            .split_terminator(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .map(str::to_lowercase)
            .collect();

        let iterators = words.clone().into_iter().map(move |word| {
            let mut prefix2 = prefix.clone();
            prefix2.extend_from_slice(word.as_bytes());
            prefix2.push(0xff);

            let mut last_possible_id = prefix2.clone();
            last_possible_id.extend_from_slice(&u64::MAX.to_be_bytes());

            self.tokenids
                .iter_from(&last_possible_id, true) // Newest pdus first
                .take_while(move |(k, _)| k.starts_with(&prefix2))
                .map(|(key, _)| key[key.len() - size_of::<u64>()..].to_vec())
        });

        Ok(utils::common_elements(iterators, |a, b| {
            // We compare b with a because we reversed the iterator earlier
            b.cmp(a)
        })
        .map(|iter| {
            (
                iter.map(move |id| {
                    let mut pduid = prefix_clone.clone();
                    pduid.extend_from_slice(&id);
                    pduid
                }),
                words,
            )
        }))
    }

