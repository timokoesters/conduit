mod data;
use std::sync::Arc;

pub use data::Data;
use ruma::{
    api::{client::relations::get_relating_events, Direction},
    events::{relation::RelationType, TimelineEventType},
    EventId, RoomId, UInt, UserId,
};
use serde::Deserialize;

use crate::{services, PduEvent, Result};

use super::timeline::PduCount;

pub struct Service {
    pub db: &'static dyn Data,
}

#[derive(Clone, Debug, Deserialize)]
struct ExtractRelType {
    rel_type: RelationType,
}
#[derive(Clone, Debug, Deserialize)]
struct ExtractRelatesToEventId {
    #[serde(rename = "m.relates_to")]
    relates_to: ExtractRelType,
}

impl Service {
    #[tracing::instrument(skip(self, from, to))]
    pub fn add_relation(&self, from: PduCount, to: PduCount) -> Result<()> {
        match (from, to) {
            (PduCount::Normal(f), PduCount::Normal(t)) => self.db.add_relation(f, t),
            _ => {
                // TODO: Relations with backfilled pdus

                Ok(())
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn paginate_relations_with_filter(
        &self,
        sender_user: &UserId,
        room_id: &RoomId,
        target: &EventId,
        filter_event_type: Option<TimelineEventType>,
        filter_rel_type: Option<RelationType>,
        from: Option<String>,
        to: Option<String>,
        limit: Option<UInt>,
        recurse: bool,
        dir: &Direction,
    ) -> Result<get_relating_events::v1::Response> {
        let from = match from {
            Some(from) => PduCount::try_from_string(&from)?,
            None => match dir {
                Direction::Forward => PduCount::min(),
                Direction::Backward => PduCount::max(),
            },
        };

        let to = to.as_ref().and_then(|t| PduCount::try_from_string(t).ok());

        // Use limit or else 10, with maximum 100
        let limit = limit
            .and_then(|u| u32::try_from(u).ok())
            .map_or(10_usize, |u| u as usize)
            .min(100);

        let next_token;

        // Spec (v1.10) recommends depth of at least 3
        let depth: u8 = if recurse { 3 } else { 1 };

        match dir {
            Direction::Forward => {
                let relations_until = &services().rooms.pdu_metadata.relations_until(
                    sender_user,
                    room_id,
                    target,
                    from,
                    depth,
                )?;
                let events_after: Vec<_> = relations_until // TODO: should be relations_after
                    .iter()
                    .filter(|(_, pdu)| {
                        filter_event_type.as_ref().map_or(true, |t| &pdu.kind == t)
                            && if let Ok(content) =
                                serde_json::from_str::<ExtractRelatesToEventId>(pdu.content.get())
                            {
                                filter_rel_type
                                    .as_ref()
                                    .map_or(true, |r| &content.relates_to.rel_type == r)
                            } else {
                                false
                            }
                    })
                    .take(limit)
                    .filter(|(_, pdu)| {
                        services()
                            .rooms
                            .state_accessor
                            .user_can_see_event(sender_user, room_id, &pdu.event_id)
                            .unwrap_or(false)
                    })
                    .take_while(|(k, _)| Some(k) != to.as_ref()) // Stop at `to`
                    .collect();

                next_token = events_after.last().map(|(count, _)| count).copied();

                let events_after: Vec<_> = events_after
                    .into_iter()
                    .rev() // relations are always most recent first
                    .map(|(_, pdu)| pdu.to_message_like_event())
                    .collect();

                Ok(get_relating_events::v1::Response {
                    chunk: events_after,
                    next_batch: next_token.map(|t| t.stringify()),
                    prev_batch: Some(from.stringify()),
                    recursion_depth: if recurse { Some(depth.into()) } else { None },
                })
            }
            Direction::Backward => {
                let relations_until = &services().rooms.pdu_metadata.relations_until(
                    sender_user,
                    room_id,
                    target,
                    from,
                    depth,
                )?;
                let events_before: Vec<_> = relations_until
                    .iter()
                    .filter(|(_, pdu)| {
                        filter_event_type.as_ref().map_or(true, |t| &pdu.kind == t)
                            && if let Ok(content) =
                                serde_json::from_str::<ExtractRelatesToEventId>(pdu.content.get())
                            {
                                filter_rel_type
                                    .as_ref()
                                    .map_or(true, |r| &content.relates_to.rel_type == r)
                            } else {
                                false
                            }
                    })
                    .take(limit)
                    .filter(|(_, pdu)| {
                        services()
                            .rooms
                            .state_accessor
                            .user_can_see_event(sender_user, room_id, &pdu.event_id)
                            .unwrap_or(false)
                    })
                    .take_while(|&(k, _)| Some(k) != to.as_ref()) // Stop at `to`
                    .collect();

                next_token = events_before.last().map(|(count, _)| count).copied();

                let events_before: Vec<_> = events_before
                    .into_iter()
                    .map(|(_, pdu)| pdu.to_message_like_event())
                    .collect();

                Ok(get_relating_events::v1::Response {
                    chunk: events_before,
                    next_batch: next_token.map(|t| t.stringify()),
                    prev_batch: Some(from.stringify()),
                    recursion_depth: if recurse { Some(depth.into()) } else { None },
                })
            }
        }
    }

    pub fn relations_until<'a>(
        &'a self,
        user_id: &'a UserId,
        room_id: &'a RoomId,
        target: &'a EventId,
        until: PduCount,
        max_depth: u8,
    ) -> Result<Vec<(PduCount, PduEvent)>> {
        let room_id = services().rooms.short.get_or_create_shortroomid(room_id)?;
        let target = match services().rooms.timeline.get_pdu_count(target)? {
            Some(PduCount::Normal(c)) => c,
            // TODO: Support backfilled relations
            _ => 0, // This will result in an empty iterator
        };

        self.db
            .relations_until(user_id, room_id, target, until)
            .map(|mut relations| {
                let mut pdus: Vec<_> = (*relations).into_iter().filter_map(Result::ok).collect();
                let mut stack: Vec<_> =
                    pdus.clone().iter().map(|pdu| (pdu.to_owned(), 1)).collect();

                while let Some(stack_pdu) = stack.pop() {
                    let target = match stack_pdu.0 .0 {
                        PduCount::Normal(c) => c,
                        // TODO: Support backfilled relations
                        PduCount::Backfilled(_) => 0, // This will result in an empty iterator
                    };

                    if let Ok(relations) = self.db.relations_until(user_id, room_id, target, until)
                    {
                        for relation in relations.flatten() {
                            if stack_pdu.1 < max_depth {
                                stack.push((relation.clone(), stack_pdu.1 + 1));
                            }

                            pdus.push(relation);
                        }
                    }
                }

                pdus.sort_by(|a, b| a.0.cmp(&b.0));
                pdus
            })
    }

    #[tracing::instrument(skip(self, room_id, event_ids))]
    pub fn mark_as_referenced(&self, room_id: &RoomId, event_ids: &[Arc<EventId>]) -> Result<()> {
        self.db.mark_as_referenced(room_id, event_ids)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_event_referenced(&self, room_id: &RoomId, event_id: &EventId) -> Result<bool> {
        self.db.is_event_referenced(room_id, event_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn mark_event_soft_failed(&self, event_id: &EventId) -> Result<()> {
        self.db.mark_event_soft_failed(event_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_event_soft_failed(&self, event_id: &EventId) -> Result<bool> {
        self.db.is_event_soft_failed(event_id)
    }
}
