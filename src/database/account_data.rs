use crate::{utils, Error, Result};
use ruma::{
    api::client::error::ErrorKind,
    events::{AnyEvent as EduEvent, EventType},
    Raw, RoomId, UserId,
};
use serde::{de::DeserializeOwned, Serialize};
use sled::IVec;
use std::{collections::HashMap, convert::TryFrom};

pub struct AccountData {
    pub(super) roomuserdataid_accountdata: sled::Tree, // RoomUserDataId = Room + User + Count + Type
}

impl AccountData {
    /// Places one event in the account data of the user and removes the previous entry.
    pub fn update<T: Serialize>(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        event_type: EventType,
        data: &T,
        globals: &super::globals::Globals<'_>,
    ) -> Result<()> {
        let mut prefix = room_id
            .map(|r| r.to_string())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&user_id.to_string().as_bytes());
        prefix.push(0xff);

        // Remove old entry
        if let Some(previous) = self.find_event(room_id, user_id, &event_type) {
            let (old_key, _) = previous?;
            self.roomuserdataid_accountdata.remove(old_key)?;
        }

        let mut key = prefix;
        key.extend_from_slice(&globals.next_count()?.to_be_bytes());
        key.push(0xff);
        key.extend_from_slice(event_type.to_string().as_bytes());

        let json = serde_json::to_value(data).expect("all types here can be serialized"); // TODO: maybe add error handling
        if json.get("type").is_none() || json.get("content").is_none() {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Account data doesn't have all required fields.",
            ));
        }

        self.roomuserdataid_accountdata
            .insert(key, &*json.to_string())?;

        Ok(())
    }

    /// Searches the account data for a specific kind.
    pub fn get<T: DeserializeOwned>(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        kind: EventType,
    ) -> Result<Option<T>> {
        self.find_event(room_id, user_id, &kind)
            .map(|r| {
                let (_, v) = r?;
                serde_json::from_slice(&v).map_err(|_| Error::bad_database("could not deserialize"))
            })
            .transpose()
    }

    /// Returns all changes to the account data that happened after `since`.
    pub fn changes_since(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        since: u64,
    ) -> Result<HashMap<EventType, Raw<EduEvent>>> {
        let mut userdata = HashMap::new();

        let mut prefix = room_id
            .map(|r| r.to_string())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&user_id.to_string().as_bytes());
        prefix.push(0xff);

        // Skip the data that's exactly at since, because we sent that last time
        let mut first_possible = prefix.clone();
        first_possible.extend_from_slice(&(since + 1).to_be_bytes());

        for r in self
            .roomuserdataid_accountdata
            .range(&*first_possible..)
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(|(k, v)| {
                Ok::<_, Error>((
                    EventType::try_from(
                        utils::string_from_bytes(k.rsplit(|&b| b == 0xff).next().ok_or_else(
                            || Error::bad_database("RoomUserData ID in db is invalid."),
                        )?)
                        .map_err(|_| Error::bad_database("RoomUserData ID in db is invalid."))?,
                    )
                    .map_err(|_| Error::bad_database("RoomUserData ID in db is invalid."))?,
                    serde_json::from_slice::<Raw<EduEvent>>(&v).map_err(|_| {
                        Error::bad_database("Database contains invalid account data.")
                    })?,
                ))
            })
        {
            let (kind, data) = r?;
            userdata.insert(kind, data);
        }

        Ok(userdata)
    }

    fn find_event(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        kind: &EventType,
    ) -> Option<Result<(IVec, IVec)>> {
        let mut prefix = room_id
            .map(|r| r.to_string())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&user_id.to_string().as_bytes());
        prefix.push(0xff);
        let kind = kind.clone();

        self.roomuserdataid_accountdata
            .scan_prefix(prefix)
            .rev()
            .find(move |r| {
                r.as_ref()
                    .map(|(k, _)| {
                        k.rsplit(|&b| b == 0xff)
                            .next()
                            .map(|current_event_type| {
                                current_event_type == kind.to_string().as_bytes()
                            })
                            .unwrap_or(false)
                    })
                    .unwrap_or(false)
            })
            .map(|r| Ok(r?))
    }
}
