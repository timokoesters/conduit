use std::sync::Arc;

use ruma::{EventId, RoomId, UserId};

use crate::{
    database::KeyValueDatabase,
    service::{self, rooms::timeline::PduCount},
    services, utils, Error, PduEvent, Result,
};

impl service::rooms::pdu_metadata::Data for KeyValueDatabase {
    fn add_relation(&self, from: u64, to: u64) -> Result<()> {
        let mut key = to.to_be_bytes().to_vec();
        key.extend_from_slice(&from.to_be_bytes());
        self.tofrom_relation.insert(&key, &[])?;
        Ok(())
    }

    fn relations_until<'a>(
        &'a self,
        user_id: &'a UserId,
        shortroomid: u64,
        target: u64,
        until: PduCount,
    ) -> Result<Box<dyn Iterator<Item = Result<(PduCount, PduEvent)>> + 'a>> {
        let prefix = target.to_be_bytes().to_vec();
        let mut current = prefix.clone();

        let count_raw = match until {
            PduCount::Normal(x) => x - 1,
            PduCount::Backfilled(x) => {
                current.extend_from_slice(&0_u64.to_be_bytes());
                u64::MAX - x - 1
            }
        };
        current.extend_from_slice(&count_raw.to_be_bytes());

        Ok(Box::new(
            self.tofrom_relation
                .iter_from(&current, true)
                .take_while(move |(k, _)| k.starts_with(&prefix))
                .map(move |(tofrom, _data)| {
                    let from = utils::u64_from_bytes(&tofrom[(size_of::<u64>())..])
                        .map_err(|_| Error::bad_database("Invalid count in tofrom_relation."))?;

                    let mut pduid = shortroomid.to_be_bytes().to_vec();
                    pduid.extend_from_slice(&from.to_be_bytes());

                    let mut pdu = services()
                        .rooms
                        .timeline
                        .get_pdu_from_id(&pduid)?
                        .ok_or_else(|| Error::bad_database("Pdu in tofrom_relation is invalid."))?;
                    if pdu.sender != user_id {
                        pdu.remove_transaction_id()?;
                    }
                    Ok((PduCount::Normal(from), pdu))
                }),
        ))
    }

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
