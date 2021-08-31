use crate::{database::DatabaseGuard, ConduitResult, Ruma};
use ruma::api::client::r0::user_directory::search_users;

#[cfg(feature = "conduit_bin")]
use rocket::post;

/// # `POST /_matrix/client/r0/user_directory/search`
///
/// Searches all known users for a match.
///
/// - TODO: Hide users that are not in any public rooms?
#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/user_directory/search", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn search_users_route(
    db: DatabaseGuard,
    body: Ruma<search_users::Request<'_>>,
) -> ConduitResult<search_users::Response> {
    let limit = u64::from(body.limit) as usize;

    let mut users = db.users.iter().filter_map(|user_id| {
        // Filter out buggy users (they should not exist, but you never know...)
        let user_id = user_id.ok()?;

        let user = search_users::User {
            user_id: user_id.clone(),
            display_name: db.users.displayname(&user_id).ok()?,
            avatar_url: db.users.avatar_url(&user_id).ok()?,
        };

        let user_id_matches = user
            .user_id
            .to_string()
            .to_lowercase()
            .contains(&body.search_term.to_lowercase());

        let user_displayname_matches = user
            .display_name
            .as_ref()
            .filter(|name| {
                name.to_lowercase()
                    .contains(&body.search_term.to_lowercase())
            })
            .is_some();

        if !user_id_matches && !user_displayname_matches {
            return None;
        }

        Some(user)
    });

    let results = users.by_ref().take(limit).collect();
    let limited = users.next().is_some();

    Ok(search_users::Response { results, limited }.into())
}
