use crate::{utils, Error, Result};
use ruma::{
    api::client::error::ErrorKind,
    events::{AnyEphemeralRoomEvent, EventType},
    serde::Raw,
    RoomId, UserId,
};
use serde::{de::DeserializeOwned, Serialize};
use std::{collections::HashMap, convert::TryFrom, sync::Arc};

use super::abstraction::Tree;

pub struct AccountData {
    pub(super) roomuserdataid_accountdata: Arc<dyn Tree>, // RoomUserDataId = Room + User + Count + Type
}

impl AccountData {
    /// Places one event in the account data of the user and removes the previous entry.
    pub fn update<T: Serialize>(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        event_type: EventType,
        data: &T,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let mut prefix = room_id
            .map(|r| r.to_string())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&user_id.as_bytes());
        prefix.push(0xff);

        // Remove old entry
        if let Some((old_key, _)) = self.find_event(room_id, user_id, &event_type)? {
            self.roomuserdataid_accountdata.remove(&old_key)?;
        }

        let mut key = prefix;
        key.extend_from_slice(&globals.next_count()?.to_be_bytes());
        key.push(0xff);
        key.extend_from_slice(event_type.as_ref().as_bytes());

        let json = serde_json::to_value(data).expect("all types here can be serialized"); // TODO: maybe add error handling
        if json.get("type").is_none() || json.get("content").is_none() {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Account data doesn't have all required fields.",
            ));
        }

        self.roomuserdataid_accountdata.insert(
            &key,
            &serde_json::to_vec(&json).expect("to_vec always works on json values"),
        )?;

        Ok(())
    }

    /// Searches the account data for a specific kind.
    pub fn get<T: DeserializeOwned>(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        kind: EventType,
    ) -> Result<Option<T>> {
        self.find_event(room_id, user_id, &kind)?
            .map(|(_, v)| {
                serde_json::from_slice(&v).map_err(|_| Error::bad_database("could not deserialize"))
            })
            .transpose()
    }

    /// Returns all changes to the account data that happened after `since`.
    #[tracing::instrument(skip(self))]
    pub fn changes_since(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        since: u64,
    ) -> Result<HashMap<EventType, Raw<AnyEphemeralRoomEvent>>> {
        let mut userdata = HashMap::new();

        let mut prefix = room_id
            .map(|r| r.to_string())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&user_id.as_bytes());
        prefix.push(0xff);

        // Skip the data that's exactly at since, because we sent that last time
        let mut first_possible = prefix.clone();
        first_possible.extend_from_slice(&(since + 1).to_be_bytes());

        for r in self
            .roomuserdataid_accountdata
            .iter_from(&first_possible, false)
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
                    serde_json::from_slice::<Raw<AnyEphemeralRoomEvent>>(&v).map_err(|_| {
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
    ) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        let mut prefix = room_id
            .map(|r| r.to_string())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&user_id.as_bytes());
        prefix.push(0xff);

        let mut last_possible_key = prefix.clone();
        last_possible_key.extend_from_slice(&u64::MAX.to_be_bytes());

        let kind = kind.clone();

        Ok(self
            .roomuserdataid_accountdata
            .iter_from(&last_possible_key, true)
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .find(move |(k, _)| {
                k.rsplit(|&b| b == 0xff)
                    .next()
                    .map(|current_event_type| current_event_type == kind.as_ref().as_bytes())
                    .unwrap_or(false)
            }))
    }
}
