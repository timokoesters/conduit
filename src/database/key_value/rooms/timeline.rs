use std::{collections::hash_map, mem::size_of, sync::Arc};

use ruma::{
    api::client::error::ErrorKind, signatures::CanonicalJsonObject, EventId, RoomId, UserId,
};
use tracing::error;

use crate::{database::KeyValueDatabase, service, services, utils, Error, PduEvent, Result};

impl service::rooms::timeline::Data for KeyValueDatabase {
    fn first_pdu_in_room(&self, room_id: &RoomId) -> Result<Option<Arc<PduEvent>>> {
        let prefix = services()
            .rooms
            .short
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();

        // Look for PDUs in that room.
        self.pduid_pdu
            .iter_from(&prefix, false)
            .filter(|(k, _)| k.starts_with(&prefix))
            .map(|(_, pdu)| {
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid first PDU in db."))
                    .map(Arc::new)
            })
            .next()
            .transpose()
    }

    fn last_timeline_count(&self, sender_user: &UserId, room_id: &RoomId) -> Result<u64> {
        match self
            .lasttimelinecount_cache
            .lock()
            .unwrap()
            .entry(room_id.to_owned())
        {
            hash_map::Entry::Vacant(v) => {
                if let Some(last_count) = self
                    .pdus_until(&sender_user, &room_id, u64::MAX)?
                    .filter_map(|r| {
                        // Filter out buggy events
                        if r.is_err() {
                            error!("Bad pdu in pdus_since: {:?}", r);
                        }
                        r.ok()
                    })
                    .map(|(pduid, _)| self.pdu_count(&pduid))
                    .next()
                {
                    Ok(*v.insert(last_count?))
                } else {
                    Ok(0)
                }
            }
            hash_map::Entry::Occupied(o) => Ok(*o.get()),
        }
    }

    /// Returns the `count` of this pdu's id.
    fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map(|pdu_id| self.pdu_count(&pdu_id))
            .transpose()
    }

    /// Returns the json of a pdu.
    fn get_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map_or_else(
                || self.eventid_outlierpdu.get(event_id.as_bytes()),
                |pduid| {
                    Ok(Some(self.pduid_pdu.get(&pduid)?.ok_or_else(|| {
                        Error::bad_database("Invalid pduid in eventid_pduid.")
                    })?))
                },
            )?
            .map(|pdu| {
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
            .transpose()
    }

    /// Returns the json of a pdu.
    fn get_non_outlier_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map(|pduid| {
                self.pduid_pdu
                    .get(&pduid)?
                    .ok_or_else(|| Error::bad_database("Invalid pduid in eventid_pduid."))
            })
            .transpose()?
            .map(|pdu| {
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
            .transpose()
    }

    /// Returns the pdu's id.
    fn get_pdu_id(&self, event_id: &EventId) -> Result<Option<Vec<u8>>> {
        self.eventid_pduid.get(event_id.as_bytes())
    }

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map(|pduid| {
                self.pduid_pdu
                    .get(&pduid)?
                    .ok_or_else(|| Error::bad_database("Invalid pduid in eventid_pduid."))
            })
            .transpose()?
            .map(|pdu| {
                serde_json::from_slice(&pdu).map_err(|_| Error::bad_database("Invalid PDU in db."))
            })
            .transpose()
    }

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    fn get_pdu(&self, event_id: &EventId) -> Result<Option<Arc<PduEvent>>> {
        if let Some(p) = self.pdu_cache.lock().unwrap().get_mut(event_id) {
            return Ok(Some(Arc::clone(p)));
        }

        if let Some(pdu) = self
            .eventid_pduid
            .get(event_id.as_bytes())?
            .map_or_else(
                || self.eventid_outlierpdu.get(event_id.as_bytes()),
                |pduid| {
                    Ok(Some(self.pduid_pdu.get(&pduid)?.ok_or_else(|| {
                        Error::bad_database("Invalid pduid in eventid_pduid.")
                    })?))
                },
            )?
            .map(|pdu| {
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))
                    .map(Arc::new)
            })
            .transpose()?
        {
            self.pdu_cache
                .lock()
                .unwrap()
                .insert(event_id.to_owned(), Arc::clone(&pdu));
            Ok(Some(pdu))
        } else {
            Ok(None)
        }
    }

    /// Returns the pdu.
    ///
    /// This does __NOT__ check the outliers `Tree`.
    fn get_pdu_from_id(&self, pdu_id: &[u8]) -> Result<Option<PduEvent>> {
        self.pduid_pdu.get(pdu_id)?.map_or(Ok(None), |pdu| {
            Ok(Some(
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
    fn get_pdu_json_from_id(&self, pdu_id: &[u8]) -> Result<Option<CanonicalJsonObject>> {
        self.pduid_pdu.get(pdu_id)?.map_or(Ok(None), |pdu| {
            Ok(Some(
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Returns the `count` of this pdu's id.
    fn pdu_count(&self, pdu_id: &[u8]) -> Result<u64> {
        utils::u64_from_bytes(&pdu_id[pdu_id.len() - size_of::<u64>()..])
            .map_err(|_| Error::bad_database("PDU has invalid count bytes."))
    }

    fn append_pdu(
        &self,
        pdu_id: &[u8],
        pdu: &PduEvent,
        json: &CanonicalJsonObject,
        count: u64,
    ) -> Result<()> {
        self.pduid_pdu.insert(
            pdu_id,
            &serde_json::to_vec(json).expect("CanonicalJsonObject is always a valid"),
        )?;

        self.lasttimelinecount_cache
            .lock()
            .unwrap()
            .insert(pdu.room_id.clone(), count);

        self.eventid_pduid
            .insert(pdu.event_id.as_bytes(), &pdu_id)?;
        self.eventid_outlierpdu.remove(pdu.event_id.as_bytes())?;

        Ok(())
    }

    /// Removes a pdu and creates a new one with the same id.
    fn replace_pdu(&self, pdu_id: &[u8], pdu: &PduEvent) -> Result<()> {
        if self.pduid_pdu.get(pdu_id)?.is_some() {
            self.pduid_pdu.insert(
                pdu_id,
                &serde_json::to_vec(pdu).expect("CanonicalJsonObject is always a valid"),
            )?;
            Ok(())
        } else {
            Err(Error::BadRequest(
                ErrorKind::NotFound,
                "PDU does not exist.",
            ))
        }
    }

    /// Returns an iterator over all events in a room that happened after the event with id `since`
    /// in chronological order.
    fn pdus_since<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<Box<dyn Iterator<Item = Result<(Vec<u8>, PduEvent)>> + 'a>> {
        let prefix = services()
            .rooms
            .short
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();

        // Skip the first pdu if it's exactly at since, because we sent that last time
        let mut first_pdu_id = prefix.clone();
        first_pdu_id.extend_from_slice(&(since + 1).to_be_bytes());

        let user_id = user_id.to_owned();

        Ok(Box::new(
            self.pduid_pdu
                .iter_from(&first_pdu_id, false)
                .take_while(move |(k, _)| k.starts_with(&prefix))
                .map(move |(pdu_id, v)| {
                    let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                        .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                    if pdu.sender != user_id {
                        pdu.remove_transaction_id()?;
                    }
                    Ok((pdu_id, pdu))
                }),
        ))
    }

    /// Returns an iterator over all events and their tokens in a room that happened before the
    /// event with id `until` in reverse-chronological order.
    fn pdus_until<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        until: u64,
    ) -> Result<Box<dyn Iterator<Item = Result<(Vec<u8>, PduEvent)>> + 'a>> {
        // Create the first part of the full pdu id
        let prefix = services()
            .rooms
            .short
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();

        let mut current = prefix.clone();
        current.extend_from_slice(&(until.saturating_sub(1)).to_be_bytes()); // -1 because we don't want event at `until`

        let current: &[u8] = &current;

        let user_id = user_id.to_owned();

        Ok(Box::new(
            self.pduid_pdu
                .iter_from(current, true)
                .take_while(move |(k, _)| k.starts_with(&prefix))
                .map(move |(pdu_id, v)| {
                    let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                        .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                    if pdu.sender != user_id {
                        pdu.remove_transaction_id()?;
                    }
                    Ok((pdu_id, pdu))
                }),
        ))
    }

    fn pdus_after<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        from: u64,
    ) -> Result<Box<dyn Iterator<Item = Result<(Vec<u8>, PduEvent)>> + 'a>> {
        // Create the first part of the full pdu id
        let prefix = services()
            .rooms
            .short
            .get_shortroomid(room_id)?
            .expect("room exists")
            .to_be_bytes()
            .to_vec();

        let mut current = prefix.clone();
        current.extend_from_slice(&(from + 1).to_be_bytes()); // +1 so we don't send the base event

        let current: &[u8] = &current;

        let user_id = user_id.to_owned();

        Ok(Box::new(
            self.pduid_pdu
                .iter_from(current, false)
                .take_while(move |(k, _)| k.starts_with(&prefix))
                .map(move |(pdu_id, v)| {
                    let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                        .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                    if pdu.sender != user_id {
                        pdu.remove_transaction_id()?;
                    }
                    Ok((pdu_id, pdu))
                }),
        ))
    }

    fn increment_notification_counts(
        &self,
        room_id: &RoomId,
        notifies: Vec<Box<UserId>>,
        highlights: Vec<Box<UserId>>,
    ) -> Result<()> {
        let mut notifies_batch = Vec::new();
        let mut highlights_batch = Vec::new();
        for user in notifies {
            let mut userroom_id = user.as_bytes().to_vec();
            userroom_id.push(0xff);
            userroom_id.extend_from_slice(room_id.as_bytes());
            notifies_batch.push(userroom_id);
        }
        for user in highlights {
            let mut userroom_id = user.as_bytes().to_vec();
            userroom_id.push(0xff);
            userroom_id.extend_from_slice(room_id.as_bytes());
            highlights_batch.push(userroom_id);
        }

        self.userroomid_notificationcount
            .increment_batch(&mut notifies_batch.into_iter())?;
        self.userroomid_highlightcount
            .increment_batch(&mut highlights_batch.into_iter())?;
        Ok(())
    }
}
