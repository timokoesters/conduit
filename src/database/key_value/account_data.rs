use std::collections::HashMap;

use ruma::{UserId, DeviceId, signatures::CanonicalJsonValue, api::client::{uiaa::UiaaInfo, error::ErrorKind}, events::{RoomAccountDataEventType, AnyEphemeralRoomEvent}, serde::Raw, RoomId};
use serde::{Serialize, de::DeserializeOwned};

use crate::{Result, database::KeyValueDatabase, service, Error, utils, services};

impl service::account_data::Data for KeyValueDatabase {
    /// Places one event in the account data of the user and removes the previous entry.
    #[tracing::instrument(skip(self, room_id, user_id, event_type, data))]
    fn update(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        event_type: RoomAccountDataEventType,
        data: &serde_json::Value,
    ) -> Result<()> {
        let mut prefix = room_id
            .map(|r| r.to_string())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(user_id.as_bytes());
        prefix.push(0xff);

        let mut roomuserdataid = prefix.clone();
        roomuserdataid.extend_from_slice(&services().globals.next_count()?.to_be_bytes());
        roomuserdataid.push(0xff);
        roomuserdataid.extend_from_slice(event_type.to_string().as_bytes());

        let mut key = prefix;
        key.extend_from_slice(event_type.to_string().as_bytes());

        if data.get("type").is_none() || data.get("content").is_none() {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Account data doesn't have all required fields.",
            ));
        }

        self.roomuserdataid_accountdata.insert(
            &roomuserdataid,
            &serde_json::to_vec(&data).expect("to_vec always works on json values"),
        )?;

        let prev = self.roomusertype_roomuserdataid.get(&key)?;

        self.roomusertype_roomuserdataid
            .insert(&key, &roomuserdataid)?;

        // Remove old entry
        if let Some(prev) = prev {
            self.roomuserdataid_accountdata.remove(&prev)?;
        }

        Ok(())
    }

    /// Searches the account data for a specific kind.
    #[tracing::instrument(skip(self, room_id, user_id, kind))]
    fn get(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        kind: RoomAccountDataEventType,
    ) -> Result<Option<Box<serde_json::value::RawValue>>> {
        let mut key = room_id
            .map(|r| r.to_string())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());
        key.push(0xff);
        key.extend_from_slice(kind.to_string().as_bytes());

        self.roomusertype_roomuserdataid
            .get(&key)?
            .and_then(|roomuserdataid| {
                self.roomuserdataid_accountdata
                    .get(&roomuserdataid)
                    .transpose()
            })
            .transpose()?
            .map(|data| {
                serde_json::from_slice(&data)
                    .map_err(|_| Error::bad_database("could not deserialize"))
            })
            .transpose()
    }

    /// Returns all changes to the account data that happened after `since`.
    #[tracing::instrument(skip(self, room_id, user_id, since))]
    fn changes_since(
        &self,
        room_id: Option<&RoomId>,
        user_id: &UserId,
        since: u64,
    ) -> Result<HashMap<RoomAccountDataEventType, Raw<AnyEphemeralRoomEvent>>> {
        let mut userdata = HashMap::new();

        let mut prefix = room_id
            .map(|r| r.to_string())
            .unwrap_or_default()
            .as_bytes()
            .to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(user_id.as_bytes());
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
                    RoomAccountDataEventType::try_from(
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
}
