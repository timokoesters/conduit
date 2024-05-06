mod data;

use std::collections::BTreeMap;

pub use data::Data;
use ruma::{
    api::client::{error::ErrorKind, threads::get_threads::v1::IncludeThreads},
    events::relation::BundledThread,
    uint, CanonicalJsonValue, EventId, RoomId, UserId,
};

use serde_json::json;

use crate::{services, Error, PduEvent, Result};

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    pub fn threads_until<'a>(
        &'a self,
        user_id: &'a UserId,
        room_id: &'a RoomId,
        until: u64,
        include: &'a IncludeThreads,
    ) -> Result<impl Iterator<Item = Result<(u64, PduEvent)>> + 'a> {
        self.db.threads_until(user_id, room_id, until, include)
    }

    pub fn add_to_thread(&self, root_event_id: &EventId, pdu: &PduEvent) -> Result<()> {
        let root_id = &services()
            .rooms
            .timeline
            .get_pdu_id(root_event_id)?
            .ok_or_else(|| {
                Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Invalid event id in thread message",
                )
            })?;

        let root_pdu = services()
            .rooms
            .timeline
            .get_pdu_from_id(root_id)?
            .ok_or_else(|| {
                Error::BadRequest(ErrorKind::InvalidParam, "Thread root pdu not found")
            })?;

        let mut root_pdu_json = services()
            .rooms
            .timeline
            .get_pdu_json_from_id(root_id)?
            .ok_or_else(|| {
                Error::BadRequest(ErrorKind::InvalidParam, "Thread root pdu not found")
            })?;

        if let CanonicalJsonValue::Object(unsigned) = root_pdu_json
            .entry("unsigned".to_owned())
            .or_insert_with(|| CanonicalJsonValue::Object(BTreeMap::default()))
        {
            if let Some(mut relations) = unsigned
                .get("m.relations")
                .and_then(|r| r.as_object())
                .and_then(|r| r.get("m.thread"))
                .and_then(|relations| {
                    serde_json::from_value::<BundledThread>(relations.clone().into()).ok()
                })
            {
                // Thread already existed
                relations.count += uint!(1);
                relations.latest_event = pdu.to_message_like_event();

                let content = serde_json::to_value(relations).expect("to_value always works");

                unsigned.insert(
                    "m.relations".to_owned(),
                    json!({ "m.thread": content })
                        .try_into()
                        .expect("thread is valid json"),
                );
            } else {
                // New thread
                let relations = BundledThread {
                    latest_event: pdu.to_message_like_event(),
                    count: uint!(1),
                    current_user_participated: true,
                };

                let content = serde_json::to_value(relations).expect("to_value always works");

                unsigned.insert(
                    "m.relations".to_owned(),
                    json!({ "m.thread": content })
                        .try_into()
                        .expect("thread is valid json"),
                );
            }

            services()
                .rooms
                .timeline
                .replace_pdu(root_id, &root_pdu_json, &root_pdu)?;
        }

        let mut users = Vec::new();
        if let Some(userids) = self.db.get_participants(root_id)? {
            users.extend_from_slice(&userids);
            users.push(pdu.sender.clone());
        } else {
            users.push(root_pdu.sender);
            users.push(pdu.sender.clone());
        }

        self.db.update_participants(root_id, &users)
    }
}
