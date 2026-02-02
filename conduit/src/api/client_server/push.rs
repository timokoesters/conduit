use crate::{services, Error, Result, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        push::{
            delete_pushrule, get_pushers, get_pushrule, get_pushrule_actions, get_pushrule_enabled,
            get_pushrules_all, set_pusher, set_pushrule, set_pushrule_actions,
            set_pushrule_enabled,
        },
    },
    events::{push_rules::PushRulesEvent, GlobalAccountDataEventType},
    push::{InsertPushRuleError, RemovePushRuleError},
};

/// # `GET /_matrix/client/r0/pushrules`
///
/// Retrieves the push rules event for this user.
pub async fn get_pushrules_all_route(
    body: Ruma<get_pushrules_all::v3::Request>,
) -> Result<get_pushrules_all::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = services()
        .account_data
        .get(
            None,
            sender_user,
            GlobalAccountDataEventType::PushRules.to_string().into(),
        )?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let account_data = serde_json::from_str::<PushRulesEvent>(event.get())
        .map_err(|_| Error::bad_database("Invalid account data event in db."))?
        .content;

    Ok(get_pushrules_all::v3::Response {
        global: account_data.global,
    })
}

/// # `GET /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}`
///
/// Retrieves a single specified push rule for this user.
pub async fn get_pushrule_route(
    body: Ruma<get_pushrule::v3::Request>,
) -> Result<get_pushrule::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = services()
        .account_data
        .get(
            None,
            sender_user,
            GlobalAccountDataEventType::PushRules.to_string().into(),
        )?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let account_data = serde_json::from_str::<PushRulesEvent>(event.get())
        .map_err(|_| Error::bad_database("Invalid account data event in db."))?
        .content;

    let rule = account_data
        .global
        .get(body.kind.clone(), &body.rule_id)
        .map(Into::into);

    if let Some(rule) = rule {
        Ok(get_pushrule::v3::Response { rule })
    } else {
        Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Push rule not found.",
        ))
    }
}

/// # `PUT /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}`
///
/// Creates a single specified push rule for this user.
pub async fn set_pushrule_route(
    body: Ruma<set_pushrule::v3::Request>,
) -> Result<set_pushrule::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let body = body.body;

    let event = services()
        .account_data
        .get(
            None,
            sender_user,
            GlobalAccountDataEventType::PushRules.to_string().into(),
        )?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let mut account_data = serde_json::from_str::<PushRulesEvent>(event.get())
        .map_err(|_| Error::bad_database("Invalid account data event in db."))?;

    if let Err(error) = account_data.content.global.insert(
        body.rule.clone(),
        body.after.as_deref(),
        body.before.as_deref(),
    ) {
        let err = match error {
            InsertPushRuleError::ServerDefaultRuleId => Error::BadRequest(
                ErrorKind::InvalidParam,
                "Rule IDs starting with a dot are reserved for server-default rules.",
            ),
            InsertPushRuleError::InvalidRuleId => Error::BadRequest(
                ErrorKind::InvalidParam,
                "Rule ID containing invalid characters.",
            ),
            InsertPushRuleError::RelativeToServerDefaultRule => Error::BadRequest(
                ErrorKind::InvalidParam,
                "Can't place a push rule relatively to a server-default rule.",
            ),
            InsertPushRuleError::UnknownRuleId => Error::BadRequest(
                ErrorKind::NotFound,
                "The before or after rule could not be found.",
            ),
            InsertPushRuleError::BeforeHigherThanAfter => Error::BadRequest(
                ErrorKind::InvalidParam,
                "The before rule has a higher priority than the after rule.",
            ),
            _ => Error::BadRequest(ErrorKind::InvalidParam, "Invalid data."),
        };

        return Err(err);
    }

    services().account_data.update(
        None,
        sender_user,
        GlobalAccountDataEventType::PushRules.to_string().into(),
        &serde_json::to_value(account_data).expect("to json value always works"),
    )?;

    Ok(set_pushrule::v3::Response {})
}

/// # `GET /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}/actions`
///
/// Gets the actions of a single specified push rule for this user.
pub async fn get_pushrule_actions_route(
    body: Ruma<get_pushrule_actions::v3::Request>,
) -> Result<get_pushrule_actions::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = services()
        .account_data
        .get(
            None,
            sender_user,
            GlobalAccountDataEventType::PushRules.to_string().into(),
        )?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let account_data = serde_json::from_str::<PushRulesEvent>(event.get())
        .map_err(|_| Error::bad_database("Invalid account data event in db."))?
        .content;

    let global = account_data.global;
    let actions = global
        .get(body.kind.clone(), &body.rule_id)
        .map(|rule| rule.actions().to_owned())
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Push rule not found.",
        ))?;

    Ok(get_pushrule_actions::v3::Response { actions })
}

/// # `PUT /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}/actions`
///
/// Sets the actions of a single specified push rule for this user.
pub async fn set_pushrule_actions_route(
    body: Ruma<set_pushrule_actions::v3::Request>,
) -> Result<set_pushrule_actions::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = services()
        .account_data
        .get(
            None,
            sender_user,
            GlobalAccountDataEventType::PushRules.to_string().into(),
        )?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let mut account_data = serde_json::from_str::<PushRulesEvent>(event.get())
        .map_err(|_| Error::bad_database("Invalid account data event in db."))?;

    if account_data
        .content
        .global
        .set_actions(body.kind.clone(), &body.rule_id, body.actions.clone())
        .is_err()
    {
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Push rule not found.",
        ));
    }

    services().account_data.update(
        None,
        sender_user,
        GlobalAccountDataEventType::PushRules.to_string().into(),
        &serde_json::to_value(account_data).expect("to json value always works"),
    )?;

    Ok(set_pushrule_actions::v3::Response {})
}

/// # `GET /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}/enabled`
///
/// Gets the enabled status of a single specified push rule for this user.
pub async fn get_pushrule_enabled_route(
    body: Ruma<get_pushrule_enabled::v3::Request>,
) -> Result<get_pushrule_enabled::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = services()
        .account_data
        .get(
            None,
            sender_user,
            GlobalAccountDataEventType::PushRules.to_string().into(),
        )?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let account_data = serde_json::from_str::<PushRulesEvent>(event.get())
        .map_err(|_| Error::bad_database("Invalid account data event in db."))?;

    let global = account_data.content.global;
    let enabled = global
        .get(body.kind.clone(), &body.rule_id)
        .map(|r| r.enabled())
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Push rule not found.",
        ))?;

    Ok(get_pushrule_enabled::v3::Response { enabled })
}

/// # `PUT /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}/enabled`
///
/// Sets the enabled status of a single specified push rule for this user.
pub async fn set_pushrule_enabled_route(
    body: Ruma<set_pushrule_enabled::v3::Request>,
) -> Result<set_pushrule_enabled::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = services()
        .account_data
        .get(
            None,
            sender_user,
            GlobalAccountDataEventType::PushRules.to_string().into(),
        )?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let mut account_data = serde_json::from_str::<PushRulesEvent>(event.get())
        .map_err(|_| Error::bad_database("Invalid account data event in db."))?;

    if account_data
        .content
        .global
        .set_enabled(body.kind.clone(), &body.rule_id, body.enabled)
        .is_err()
    {
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Push rule not found.",
        ));
    }

    services().account_data.update(
        None,
        sender_user,
        GlobalAccountDataEventType::PushRules.to_string().into(),
        &serde_json::to_value(account_data).expect("to json value always works"),
    )?;

    Ok(set_pushrule_enabled::v3::Response {})
}

/// # `DELETE /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}`
///
/// Deletes a single specified push rule for this user.
pub async fn delete_pushrule_route(
    body: Ruma<delete_pushrule::v3::Request>,
) -> Result<delete_pushrule::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = services()
        .account_data
        .get(
            None,
            sender_user,
            GlobalAccountDataEventType::PushRules.to_string().into(),
        )?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let mut account_data = serde_json::from_str::<PushRulesEvent>(event.get())
        .map_err(|_| Error::bad_database("Invalid account data event in db."))?;

    if let Err(error) = account_data
        .content
        .global
        .remove(body.kind.clone(), &body.rule_id)
    {
        let err = match error {
            RemovePushRuleError::ServerDefault => Error::BadRequest(
                ErrorKind::InvalidParam,
                "Cannot delete a server-default pushrule.",
            ),
            RemovePushRuleError::NotFound => {
                Error::BadRequest(ErrorKind::NotFound, "Push rule not found.")
            }
            _ => Error::BadRequest(ErrorKind::InvalidParam, "Invalid data."),
        };

        return Err(err);
    }

    services().account_data.update(
        None,
        sender_user,
        GlobalAccountDataEventType::PushRules.to_string().into(),
        &serde_json::to_value(account_data).expect("to json value always works"),
    )?;

    Ok(delete_pushrule::v3::Response {})
}

/// # `GET /_matrix/client/r0/pushers`
///
/// Gets all currently active pushers for the sender user.
pub async fn get_pushers_route(
    body: Ruma<get_pushers::v3::Request>,
) -> Result<get_pushers::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    Ok(get_pushers::v3::Response {
        pushers: services().pusher.get_pushers(sender_user)?,
    })
}

/// # `POST /_matrix/client/r0/pushers/set`
///
/// Adds a pusher for the sender user.
///
/// - TODO: Handle `append`
pub async fn set_pushers_route(
    body: Ruma<set_pusher::v3::Request>,
) -> Result<set_pusher::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    services()
        .pusher
        .set_pusher(sender_user, body.action.clone())?;

    Ok(set_pusher::v3::Response::default())
}
