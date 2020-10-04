use super::State;
use crate::{ConduitResult, Database, Ruma};
use ruma::api::client::r0::user_directory::search_users;

#[cfg(feature = "conduit_bin")]
use rocket::post;

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/user_directory/search", data = "<body>")
)]
pub fn search_users_route(
    db: State<'_, Database<'_>>,
    body: Ruma<search_users::Request<'_>>,
) -> ConduitResult<search_users::Response> {
    let limit = u64::from(body.limit) as usize;

    let mut users = db.users.iter().filter_map(|user_id| {
        // Filter out buggy users (they should not exist, but you never know...)
        let user_id = user_id.ok()?;
        if db.users.is_deactivated(&user_id).ok()? {
            return None;
        }

        let user = search_users::User {
            user_id: user_id.clone(),
            display_name: db.users.displayname(&user_id).ok()?,
            avatar_url: db.users.avatar_url(&user_id).ok()?,
        };

        if !user.user_id.to_string().contains(&body.search_term)
            && user
                .display_name
                .as_ref()
                .filter(|name| name.contains(&body.search_term))
                .is_none()
        {
            return None;
        }

        Some(user)
    });

    let results = users.by_ref().take(limit).collect();
    let limited = users.next().is_some();

    Ok(search_users::Response { results, limited }.into())
}
