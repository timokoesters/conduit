use std::{collections::hash_map, mem::size_of, sync::Arc};

use ruma::{
    api::client::error::ErrorKind, CanonicalJsonObject, EventId, OwnedUserId, RoomId, UserId,
};
use tracing::error;

use crate::{database::KeyValueDatabase, service, services, utils, Error, PduEvent, Result};

use service::rooms::timeline::PduCount;

use super::get_userroom_id_bytes;

impl service::rooms::timeline::Data for KeyValueDatabase {
    fn last_timeline_count(&self, sender_user: &UserId, room_id: &RoomId) -> Result<PduCount> {
        match self
            .lasttimelinecount_cache
            .lock()
            .unwrap()
            .entry(room_id.to_owned())
        {
            hash_map::Entry::Vacant(v) => {
                if let Some(last_count) = self
                    .pdus_until(sender_user, room_id, PduCount::max())?
                    .find_map(|r| {
                        // Filter out buggy events
                        if r.is_err() {
                            error!("Bad pdu in pdus_since: {:?}", r);
                        }
                        r.ok()
                    })
                {
                    Ok(*v.insert(last_count.0))
                } else {
                    Ok(PduCount::Normal(0))
                }
            }
            hash_map::Entry::Occupied(o) => Ok(*o.get()),
        }
    }

    /// Returns the `count` of this pdu's id.
    fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<PduCount>> {
        self.eventid_pduid
            .get(event_id.as_bytes())?
            .map(|pdu_id| pdu_count(&pdu_id))
            .transpose()
    }

    /// Returns the json of a pdu.
    fn get_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>> {
        self.get_non_outlier_pdu_json(event_id)?.map_or_else(
            || {
                self.eventid_outlierpdu
                    .get(event_id.as_bytes())?
                    .map(|pdu| {
                        serde_json::from_slice(&pdu)
                            .map_err(|_| Error::bad_database("Invalid PDU in db."))
                    })
                    .transpose()
            },
            |x| Ok(Some(x)),
        )
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
            .get_non_outlier_pdu(event_id)?
            .map_or_else(
                || {
                    self.eventid_outlierpdu
                        .get(event_id.as_bytes())?
                        .map(|pdu| {
                            serde_json::from_slice(&pdu)
                                .map_err(|_| Error::bad_database("Invalid PDU in db."))
                        })
                        .transpose()
                },
                |x| Ok(Some(x)),
            )?
            .map(Arc::new)
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
            .insert(pdu.room_id().into_owned(), PduCount::Normal(count));

        self.eventid_pduid.insert(pdu.event_id.as_bytes(), pdu_id)?;
        self.eventid_outlierpdu.remove(pdu.event_id.as_bytes())?;

        Ok(())
    }

    fn prepend_backfill_pdu(
        &self,
        pdu_id: &[u8],
        event_id: &EventId,
        json: &CanonicalJsonObject,
    ) -> Result<()> {
        self.pduid_pdu.insert(
            pdu_id,
            &serde_json::to_vec(json).expect("CanonicalJsonObject is always a valid"),
        )?;

        self.eventid_pduid.insert(event_id.as_bytes(), pdu_id)?;
        self.eventid_outlierpdu.remove(event_id.as_bytes())?;

        Ok(())
    }

    /// Removes a pdu and creates a new one with the same id.
    fn replace_pdu(
        &self,
        pdu_id: &[u8],
        pdu_json: &CanonicalJsonObject,
        pdu: &PduEvent,
    ) -> Result<()> {
        if self.pduid_pdu.get(pdu_id)?.is_some() {
            self.pduid_pdu.insert(
                pdu_id,
                &serde_json::to_vec(pdu_json).expect("CanonicalJsonObject is always a valid"),
            )?;
        } else {
            return Err(Error::BadRequest(
                ErrorKind::NotFound,
                "PDU does not exist.",
            ));
        }

        self.pdu_cache
            .lock()
            .unwrap()
            .remove(&(*pdu.event_id).to_owned());

        Ok(())
    }

    /// Returns an iterator over all events and their tokens in a room that happened before the
    /// event with id `until` in reverse-chronological order.
    fn pdus_until<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        until: PduCount,
    ) -> Result<Box<dyn Iterator<Item = Result<(PduCount, PduEvent)>> + 'a>> {
        let (prefix, current) = count_to_id(room_id, until, 1, true)?;

        let user_id = user_id.to_owned();

        Ok(Box::new(
            self.pduid_pdu
                .iter_from(&current, true)
                .take_while(move |(k, _)| k.starts_with(&prefix))
                .map(move |(pdu_id, v)| {
                    let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                        .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                    if pdu.sender != user_id {
                        pdu.remove_transaction_id()?;
                    }
                    pdu.add_age()?;
                    let count = pdu_count(&pdu_id)?;
                    Ok((count, pdu))
                }),
        ))
    }

    fn pdus_after<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        from: PduCount,
    ) -> Result<Box<dyn Iterator<Item = Result<(PduCount, PduEvent)>> + 'a>> {
        let (prefix, current) = count_to_id(room_id, from, 1, false)?;

        let user_id = user_id.to_owned();

        Ok(Box::new(
            self.pduid_pdu
                .iter_from(&current, false)
                .take_while(move |(k, _)| k.starts_with(&prefix))
                .map(move |(pdu_id, v)| {
                    let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                        .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                    if pdu.sender != user_id {
                        pdu.remove_transaction_id()?;
                    }
                    pdu.add_age()?;
                    let count = pdu_count(&pdu_id)?;
                    Ok((count, pdu))
                }),
        ))
    }

    fn increment_notification_counts(
        &self,
        room_id: &RoomId,
        notifies: Vec<OwnedUserId>,
        highlights: Vec<OwnedUserId>,
    ) -> Result<()> {
        let mut notifies_batch = Vec::new();
        let mut highlights_batch = Vec::new();
        for user in notifies {
            let userroom_id = get_userroom_id_bytes(&user, room_id);
            notifies_batch.push(userroom_id);
        }
        for user in highlights {
            let userroom_id = get_userroom_id_bytes(&user, room_id);
            highlights_batch.push(userroom_id);
        }

        self.userroomid_notificationcount
            .increment_batch(&mut notifies_batch.into_iter())?;
        self.userroomid_highlightcount
            .increment_batch(&mut highlights_batch.into_iter())?;
        Ok(())
    }
}

/// Returns the `count` of this pdu's id.
fn pdu_count(pdu_id: &[u8]) -> Result<PduCount> {
    let last_u64 = utils::u64_from_bytes(&pdu_id[pdu_id.len() - size_of::<u64>()..])
        .map_err(|_| Error::bad_database("PDU has invalid count bytes."))?;
    let second_last_u64 = utils::u64_from_bytes(
        &pdu_id[pdu_id.len() - 2 * size_of::<u64>()..pdu_id.len() - size_of::<u64>()],
    );

    if matches!(second_last_u64, Ok(0)) {
        Ok(PduCount::Backfilled(u64::MAX - last_u64))
    } else {
        Ok(PduCount::Normal(last_u64))
    }
}

fn count_to_id(
    room_id: &RoomId,
    count: PduCount,
    offset: u64,
    subtract: bool,
) -> Result<(Vec<u8>, Vec<u8>)> {
    let prefix = services()
        .rooms
        .short
        .get_shortroomid(room_id)?
        .ok_or_else(|| Error::bad_database("Looked for bad shortroomid in timeline"))?
        .to_be_bytes()
        .to_vec();
    let mut pdu_id = prefix.clone();
    // +1 so we don't send the base event
    let count_raw = match count {
        PduCount::Normal(x) => {
            if subtract {
                x - offset
            } else {
                x + offset
            }
        }
        PduCount::Backfilled(x) => {
            pdu_id.extend_from_slice(&0_u64.to_be_bytes());
            let num = u64::MAX - x;
            if subtract {
                if num > 0 {
                    num - offset
                } else {
                    num
                }
            } else {
                num + offset
            }
        }
    };
    pdu_id.extend_from_slice(&count_raw.to_be_bytes());

    Ok((prefix, pdu_id))
}
