use crate::{services, Error, Result, Ruma};
use ruma::api::client::{
    error::ErrorKind,
    search::search_events::{
        self,
        v3::{EventContextResult, ResultCategories, ResultRoomEvents, SearchResult},
    },
};

use std::collections::BTreeMap;

/// # `POST /_matrix/client/r0/search`
///
/// Searches rooms for messages.
///
/// - Only works if the user is currently joined to the room (TODO: Respect history visibility)
pub async fn search_events_route(
    body: Ruma<search_events::v3::IncomingRequest>,
) -> Result<search_events::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let search_criteria = body.search_categories.room_events.as_ref().unwrap();
    let filter = &search_criteria.filter;

    let room_ids = filter.rooms.clone().unwrap_or_else(|| {
        services()
            .rooms
            .state_cache
            .rooms_joined(sender_user)
            .filter_map(|r| r.ok())
            .collect()
    });

    let limit = filter.limit.map_or(10, |l| u64::from(l) as usize);

    let mut searches = Vec::new();

    for room_id in room_ids {
        if !services()
            .rooms
            .state_cache
            .is_joined(sender_user, &room_id)?
        {
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "You don't have permission to view this room.",
            ));
        }

        if let Some(search) = services()
            .rooms
            .search
            .search_pdus(&room_id, &search_criteria.search_term)?
        {
            searches.push(search.0.peekable());
        }
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

    let results: Vec<_> = results
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
                result: services()
                    .rooms
                    .timeline
                    .get_pdu_from_id(result)?
                    .map(|pdu| pdu.to_room_event()),
            })
        })
        .filter_map(|r| r.ok())
        .skip(skip)
        .take(limit)
        .collect();

    let next_batch = if results.len() < limit as usize {
        None
    } else {
        Some((skip + limit).to_string())
    };

    Ok(search_events::v3::Response::new(ResultCategories {
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
                .collect(),
        },
    }))
}
