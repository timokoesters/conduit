impl service::room::auth_chain::Data for KeyValueDatabase {
    fn get_cached_eventid_authchain<'a>() -> Result<HashSet<u64>> {
        self.shorteventid_authchain
            .get(&shorteventid.to_be_bytes())?
            .map(|chain| {
                chain
                    .chunks_exact(size_of::<u64>())
                    .map(|chunk| {
                        utils::u64_from_bytes(chunk).expect("byte length is correct")
                    })
                    .collect()
            })
    }

    fn cache_eventid_authchain<'a>(shorteventid: u64, auth_chain: &HashSet<u64>) -> Result<()> {
        shorteventid_authchain.insert(
            &shorteventid.to_be_bytes(),
            &auth_chain
                .iter()
                .flat_map(|s| s.to_be_bytes().to_vec())
                .collect::<Vec<u8>>(),
        )
    }
}
