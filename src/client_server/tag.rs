use super::State;
use crate::{ConduitResult, Database, Ruma};
use ruma::{
    api::client::r0::tag::{create_tag, delete_tag, get_tags},
    events::EventType,
};
use std::collections::BTreeMap;

#[cfg(feature = "conduit_bin")]
use rocket::{delete, get, put};

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/user/<_>/rooms/<_>/tags/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn update_tag_route(
    db: State<'_, Database>,
    body: Ruma<create_tag::Request<'_>>,
) -> ConduitResult<create_tag::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut tags_event = db
        .account_data
        .get::<ruma::events::tag::TagEvent>(Some(&body.room_id), sender_user, EventType::Tag)?
        .unwrap_or_else(|| ruma::events::tag::TagEvent {
            content: ruma::events::tag::TagEventContent {
                tags: BTreeMap::new(),
            },
        });
    tags_event
        .content
        .tags
        .insert(body.tag.to_string(), body.tag_info.clone());

    db.account_data.update(
        Some(&body.room_id),
        sender_user,
        EventType::Tag,
        &tags_event,
        &db.globals,
    )?;

    db.flush().await?;

    Ok(create_tag::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    delete("/_matrix/client/r0/user/<_>/rooms/<_>/tags/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn delete_tag_route(
    db: State<'_, Database>,
    body: Ruma<delete_tag::Request<'_>>,
) -> ConduitResult<delete_tag::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut tags_event = db
        .account_data
        .get::<ruma::events::tag::TagEvent>(Some(&body.room_id), sender_user, EventType::Tag)?
        .unwrap_or_else(|| ruma::events::tag::TagEvent {
            content: ruma::events::tag::TagEventContent {
                tags: BTreeMap::new(),
            },
        });
    tags_event.content.tags.remove(&body.tag);

    db.account_data.update(
        Some(&body.room_id),
        sender_user,
        EventType::Tag,
        &tags_event,
        &db.globals,
    )?;

    db.flush().await?;

    Ok(delete_tag::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/user/<_>/rooms/<_>/tags", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_tags_route(
    db: State<'_, Database>,
    body: Ruma<get_tags::Request<'_>>,
) -> ConduitResult<get_tags::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    Ok(get_tags::Response {
        tags: db
            .account_data
            .get::<ruma::events::tag::TagEvent>(Some(&body.room_id), sender_user, EventType::Tag)?
            .unwrap_or_else(|| ruma::events::tag::TagEvent {
                content: ruma::events::tag::TagEventContent {
                    tags: BTreeMap::new(),
                },
            })
            .content
            .tags,
    }
    .into())
}
