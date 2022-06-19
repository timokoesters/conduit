use crate::{database::DatabaseGuard, Result, Ruma};
use ruma::{
    api::client::tag::{create_tag, delete_tag, get_tags},
    events::{
        tag::{TagEvent, TagEventContent},
        RoomAccountDataEventType,
    },
};
use std::collections::BTreeMap;

/// # `PUT /_matrix/client/r0/user/{userId}/rooms/{roomId}/tags/{tag}`
///
/// Adds a tag to the room.
///
/// - Inserts the tag into the tag event of the room account data.
pub async fn update_tag_route(
    db: DatabaseGuard,
    body: Ruma<create_tag::v3::IncomingRequest>,
) -> Result<create_tag::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut tags_event = db
        .account_data
        .get(
            Some(&body.room_id),
            sender_user,
            RoomAccountDataEventType::Tag,
        )?
        .unwrap_or_else(|| TagEvent {
            content: TagEventContent {
                tags: BTreeMap::new(),
            },
        });
    tags_event
        .content
        .tags
        .insert(body.tag.clone().into(), body.tag_info.clone());

    db.account_data.update(
        Some(&body.room_id),
        sender_user,
        RoomAccountDataEventType::Tag,
        &tags_event,
        &db.globals,
    )?;

    db.flush()?;

    Ok(create_tag::v3::Response {})
}

/// # `DELETE /_matrix/client/r0/user/{userId}/rooms/{roomId}/tags/{tag}`
///
/// Deletes a tag from the room.
///
/// - Removes the tag from the tag event of the room account data.
pub async fn delete_tag_route(
    db: DatabaseGuard,
    body: Ruma<delete_tag::v3::IncomingRequest>,
) -> Result<delete_tag::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut tags_event = db
        .account_data
        .get(
            Some(&body.room_id),
            sender_user,
            RoomAccountDataEventType::Tag,
        )?
        .unwrap_or_else(|| TagEvent {
            content: TagEventContent {
                tags: BTreeMap::new(),
            },
        });
    tags_event.content.tags.remove(&body.tag.clone().into());

    db.account_data.update(
        Some(&body.room_id),
        sender_user,
        RoomAccountDataEventType::Tag,
        &tags_event,
        &db.globals,
    )?;

    db.flush()?;

    Ok(delete_tag::v3::Response {})
}

/// # `GET /_matrix/client/r0/user/{userId}/rooms/{roomId}/tags`
///
/// Returns tags on the room.
///
/// - Gets the tag event of the room account data.
pub async fn get_tags_route(
    db: DatabaseGuard,
    body: Ruma<get_tags::v3::IncomingRequest>,
) -> Result<get_tags::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    Ok(get_tags::v3::Response {
        tags: db
            .account_data
            .get(
                Some(&body.room_id),
                sender_user,
                RoomAccountDataEventType::Tag,
            )?
            .unwrap_or_else(|| TagEvent {
                content: TagEventContent {
                    tags: BTreeMap::new(),
                },
            })
            .content
            .tags,
    })
}
