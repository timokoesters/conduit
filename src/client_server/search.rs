use super::State;
use crate::{database::ReadGuard, ConduitResult, Database, Error, Ruma};
use ruma::api::client::{error::ErrorKind, r0::search::search_events};
use std::sync::Arc;

#[cfg(feature = "conduit_bin")]
use rocket::post;
use search_events::{EventContextResult, ResultCategories, ResultRoomEvents, SearchResult};
use std::collections::BTreeMap;

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/search", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn search_events_route(
    db: ReadGuard,
    body: Ruma<search_events::Request<'_>>,
) -> ConduitResult<search_events::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let search_criteria = body.search_categories.room_events.as_ref().unwrap();
    let filter = search_criteria.filter.clone().unwrap_or_default();

    let room_ids = filter.rooms.clone().unwrap_or_else(|| {
        db.rooms
            .rooms_joined(&sender_user)
            .filter_map(|r| r.ok())
            .collect()
    });

    let limit = filter.limit.map_or(10, |l| u64::from(l) as usize);

    let mut searches = Vec::new();

    for room_id in room_ids {
        if !db.rooms.is_joined(sender_user, &room_id)? {
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "You don't have permission to view this room.",
            ));
        }

        let search = db
            .rooms
            .search_pdus(&room_id, &search_criteria.search_term)?;

        searches.push(search.0.peekable());
    }

    let skip = match body.next_batch.as_ref().map(|s| s.parse()) {
        Some(Ok(s)) => s,
        Some(Err(_)) => {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Invalid next_batch token.",
            ))
        }
        None => 0, // Default to the start
    };

    let mut results = Vec::new();
    for _ in 0..skip + limit {
        if let Some(s) = searches
            .iter_mut()
            .map(|s| (s.peek().cloned(), s))
            .max_by_key(|(peek, _)| peek.clone())
            .and_then(|(_, i)| i.next())
        {
            results.push(s);
        }
    }

    let results = results
        .iter()
        .map(|result| {
            Ok::<_, Error>(SearchResult {
                context: EventContextResult {
                    end: None,
                    events_after: Vec::new(),
                    events_before: Vec::new(),
                    profile_info: BTreeMap::new(),
                    start: None,
                },
                rank: None,
                result: db
                    .rooms
                    .get_pdu_from_id(&result)?
                    .map(|pdu| pdu.to_room_event()),
            })
        })
        .filter_map(|r| r.ok())
        .skip(skip)
        .take(limit)
        .collect::<Vec<_>>();

    let next_batch = if results.len() < limit as usize {
        None
    } else {
        Some((skip + limit).to_string())
    };

    Ok(search_events::Response::new(ResultCategories {
        room_events: ResultRoomEvents {
            count: Some((results.len() as u32).into()), // TODO: set this to none. Element shouldn't depend on it
            groups: BTreeMap::new(),                    // TODO
            next_batch,
            results,
            state: BTreeMap::new(), // TODO
            highlights: search_criteria
                .search_term
                .split_terminator(|c: char| !c.is_alphanumeric())
                .map(str::to_lowercase)
                .collect::<Vec<_>>(),
        },
    })
    .into())
}
