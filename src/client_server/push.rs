use crate::{database::DatabaseGuard, ConduitResult, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::push::{
            delete_pushrule, get_pushers, get_pushrule, get_pushrule_actions, get_pushrule_enabled,
            get_pushrules_all, set_pusher, set_pushrule, set_pushrule_actions,
            set_pushrule_enabled, RuleKind,
        },
    },
    events::{push_rules, EventType},
    push::{ConditionalPushRuleInit, PatternedPushRuleInit, SimplePushRuleInit},
};

#[cfg(feature = "conduit_bin")]
use rocket::{delete, get, post, put};

/// # `GET /_matrix/client/r0/pushrules`
///
/// Retrieves the push rules event for this user.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushrules", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_pushrules_all_route(
    db: DatabaseGuard,
    body: Ruma<get_pushrules_all::Request>,
) -> ConduitResult<get_pushrules_all::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    Ok(get_pushrules_all::Response {
        global: event.content.global,
    }
    .into())
}

/// # `GET /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}`
///
/// Retrieves a single specified push rule for this user.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushrules/<_>/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_pushrule_route(
    db: DatabaseGuard,
    body: Ruma<get_pushrule::Request<'_>>,
) -> ConduitResult<get_pushrule::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = event.content.global;
    let rule = match body.kind {
        RuleKind::Override => global
            .override_
            .get(body.rule_id.as_str())
            .map(|rule| rule.clone().into()),
        RuleKind::Underride => global
            .underride
            .get(body.rule_id.as_str())
            .map(|rule| rule.clone().into()),
        RuleKind::Sender => global
            .sender
            .get(body.rule_id.as_str())
            .map(|rule| rule.clone().into()),
        RuleKind::Room => global
            .room
            .get(body.rule_id.as_str())
            .map(|rule| rule.clone().into()),
        RuleKind::Content => global
            .content
            .get(body.rule_id.as_str())
            .map(|rule| rule.clone().into()),
        _ => None,
    };

    if let Some(rule) = rule {
        Ok(get_pushrule::Response { rule }.into())
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
#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/pushrules/<_>/<_>/<_>", data = "<req>")
)]
#[tracing::instrument(skip(db, req))]
pub async fn set_pushrule_route(
    db: DatabaseGuard,
    req: Ruma<set_pushrule::Request<'_>>,
) -> ConduitResult<set_pushrule::Response> {
    let sender_user = req.sender_user.as_ref().expect("user is authenticated");
    let body = req.body;

    if body.scope != "global" {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Scopes other than 'global' are not supported.",
        ));
    }

    let mut event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    match body.kind {
        RuleKind::Override => {
            global.override_.replace(
                ConditionalPushRuleInit {
                    actions: body.actions,
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id,
                    conditions: body.conditions,
                }
                .into(),
            );
        }
        RuleKind::Underride => {
            global.underride.replace(
                ConditionalPushRuleInit {
                    actions: body.actions,
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id,
                    conditions: body.conditions,
                }
                .into(),
            );
        }
        RuleKind::Sender => {
            global.sender.replace(
                SimplePushRuleInit {
                    actions: body.actions,
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id,
                }
                .into(),
            );
        }
        RuleKind::Room => {
            global.room.replace(
                SimplePushRuleInit {
                    actions: body.actions,
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id,
                }
                .into(),
            );
        }
        RuleKind::Content => {
            global.content.replace(
                PatternedPushRuleInit {
                    actions: body.actions,
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id,
                    pattern: body.pattern.unwrap_or_default(),
                }
                .into(),
            );
        }
        _ => {}
    }

    db.account_data
        .update(None, sender_user, EventType::PushRules, &event, &db.globals)?;

    db.flush()?;

    Ok(set_pushrule::Response {}.into())
}

/// # `GET /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}/actions`
///
/// Gets the actions of a single specified push rule for this user.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushrules/<_>/<_>/<_>/actions", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_pushrule_actions_route(
    db: DatabaseGuard,
    body: Ruma<get_pushrule_actions::Request<'_>>,
) -> ConduitResult<get_pushrule_actions::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if body.scope != "global" {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Scopes other than 'global' are not supported.",
        ));
    }

    let mut event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    let actions = match body.kind {
        RuleKind::Override => global
            .override_
            .get(body.rule_id.as_str())
            .map(|rule| rule.actions.clone()),
        RuleKind::Underride => global
            .underride
            .get(body.rule_id.as_str())
            .map(|rule| rule.actions.clone()),
        RuleKind::Sender => global
            .sender
            .get(body.rule_id.as_str())
            .map(|rule| rule.actions.clone()),
        RuleKind::Room => global
            .room
            .get(body.rule_id.as_str())
            .map(|rule| rule.actions.clone()),
        RuleKind::Content => global
            .content
            .get(body.rule_id.as_str())
            .map(|rule| rule.actions.clone()),
        _ => None,
    };

    db.flush()?;

    Ok(get_pushrule_actions::Response {
        actions: actions.unwrap_or_default(),
    }
    .into())
}

/// # `PUT /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}/actions`
///
/// Sets the actions of a single specified push rule for this user.
#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/pushrules/<_>/<_>/<_>/actions", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn set_pushrule_actions_route(
    db: DatabaseGuard,
    body: Ruma<set_pushrule_actions::Request<'_>>,
) -> ConduitResult<set_pushrule_actions::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if body.scope != "global" {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Scopes other than 'global' are not supported.",
        ));
    }

    let mut event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    match body.kind {
        RuleKind::Override => {
            if let Some(mut rule) = global.override_.get(body.rule_id.as_str()).cloned() {
                rule.actions = body.actions.clone();
                global.override_.replace(rule);
            }
        }
        RuleKind::Underride => {
            if let Some(mut rule) = global.underride.get(body.rule_id.as_str()).cloned() {
                rule.actions = body.actions.clone();
                global.underride.replace(rule);
            }
        }
        RuleKind::Sender => {
            if let Some(mut rule) = global.sender.get(body.rule_id.as_str()).cloned() {
                rule.actions = body.actions.clone();
                global.sender.replace(rule);
            }
        }
        RuleKind::Room => {
            if let Some(mut rule) = global.room.get(body.rule_id.as_str()).cloned() {
                rule.actions = body.actions.clone();
                global.room.replace(rule);
            }
        }
        RuleKind::Content => {
            if let Some(mut rule) = global.content.get(body.rule_id.as_str()).cloned() {
                rule.actions = body.actions.clone();
                global.content.replace(rule);
            }
        }
        _ => {}
    };

    db.account_data
        .update(None, sender_user, EventType::PushRules, &event, &db.globals)?;

    db.flush()?;

    Ok(set_pushrule_actions::Response {}.into())
}

/// # `GET /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}/enabled`
///
/// Gets the enabled status of a single specified push rule for this user.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushrules/<_>/<_>/<_>/enabled", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_pushrule_enabled_route(
    db: DatabaseGuard,
    body: Ruma<get_pushrule_enabled::Request<'_>>,
) -> ConduitResult<get_pushrule_enabled::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if body.scope != "global" {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Scopes other than 'global' are not supported.",
        ));
    }

    let mut event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    let enabled = match body.kind {
        RuleKind::Override => global
            .override_
            .iter()
            .find(|rule| rule.rule_id == body.rule_id)
            .map_or(false, |rule| rule.enabled),
        RuleKind::Underride => global
            .underride
            .iter()
            .find(|rule| rule.rule_id == body.rule_id)
            .map_or(false, |rule| rule.enabled),
        RuleKind::Sender => global
            .sender
            .iter()
            .find(|rule| rule.rule_id == body.rule_id)
            .map_or(false, |rule| rule.enabled),
        RuleKind::Room => global
            .room
            .iter()
            .find(|rule| rule.rule_id == body.rule_id)
            .map_or(false, |rule| rule.enabled),
        RuleKind::Content => global
            .content
            .iter()
            .find(|rule| rule.rule_id == body.rule_id)
            .map_or(false, |rule| rule.enabled),
        _ => false,
    };

    db.flush()?;

    Ok(get_pushrule_enabled::Response { enabled }.into())
}

/// # `PUT /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}/enabled`
///
/// Sets the enabled status of a single specified push rule for this user.
#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/pushrules/<_>/<_>/<_>/enabled", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn set_pushrule_enabled_route(
    db: DatabaseGuard,
    body: Ruma<set_pushrule_enabled::Request<'_>>,
) -> ConduitResult<set_pushrule_enabled::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if body.scope != "global" {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Scopes other than 'global' are not supported.",
        ));
    }

    let mut event = db
        .account_data
        .get::<ruma::events::push_rules::PushRulesEvent>(None, sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    match body.kind {
        RuleKind::Override => {
            if let Some(mut rule) = global.override_.get(body.rule_id.as_str()).cloned() {
                global.override_.remove(&rule);
                rule.enabled = body.enabled;
                global.override_.insert(rule);
            }
        }
        RuleKind::Underride => {
            if let Some(mut rule) = global.underride.get(body.rule_id.as_str()).cloned() {
                global.underride.remove(&rule);
                rule.enabled = body.enabled;
                global.underride.insert(rule);
            }
        }
        RuleKind::Sender => {
            if let Some(mut rule) = global.sender.get(body.rule_id.as_str()).cloned() {
                global.sender.remove(&rule);
                rule.enabled = body.enabled;
                global.sender.insert(rule);
            }
        }
        RuleKind::Room => {
            if let Some(mut rule) = global.room.get(body.rule_id.as_str()).cloned() {
                global.room.remove(&rule);
                rule.enabled = body.enabled;
                global.room.insert(rule);
            }
        }
        RuleKind::Content => {
            if let Some(mut rule) = global.content.get(body.rule_id.as_str()).cloned() {
                global.content.remove(&rule);
                rule.enabled = body.enabled;
                global.content.insert(rule);
            }
        }
        _ => {}
    }

    db.account_data
        .update(None, sender_user, EventType::PushRules, &event, &db.globals)?;

    db.flush()?;

    Ok(set_pushrule_enabled::Response {}.into())
}

/// # `DELETE /_matrix/client/r0/pushrules/{scope}/{kind}/{ruleId}`
///
/// Deletes a single specified push rule for this user.
#[cfg_attr(
    feature = "conduit_bin",
    delete("/_matrix/client/r0/pushrules/<_>/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn delete_pushrule_route(
    db: DatabaseGuard,
    body: Ruma<delete_pushrule::Request<'_>>,
) -> ConduitResult<delete_pushrule::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if body.scope != "global" {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Scopes other than 'global' are not supported.",
        ));
    }

    let mut event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    match body.kind {
        RuleKind::Override => {
            if let Some(rule) = global.override_.get(body.rule_id.as_str()).cloned() {
                global.override_.remove(&rule);
            }
        }
        RuleKind::Underride => {
            if let Some(rule) = global.underride.get(body.rule_id.as_str()).cloned() {
                global.underride.remove(&rule);
            }
        }
        RuleKind::Sender => {
            if let Some(rule) = global.sender.get(body.rule_id.as_str()).cloned() {
                global.sender.remove(&rule);
            }
        }
        RuleKind::Room => {
            if let Some(rule) = global.room.get(body.rule_id.as_str()).cloned() {
                global.room.remove(&rule);
            }
        }
        RuleKind::Content => {
            if let Some(rule) = global.content.get(body.rule_id.as_str()).cloned() {
                global.content.remove(&rule);
            }
        }
        _ => {}
    }

    db.account_data
        .update(None, sender_user, EventType::PushRules, &event, &db.globals)?;

    db.flush()?;

    Ok(delete_pushrule::Response {}.into())
}

/// # `GET /_matrix/client/r0/pushers`
///
/// Gets all currently active pushers for the sender user.
#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushers", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_pushers_route(
    db: DatabaseGuard,
    body: Ruma<get_pushers::Request>,
) -> ConduitResult<get_pushers::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    Ok(get_pushers::Response {
        pushers: db.pusher.get_pushers(sender_user)?,
    }
    .into())
}

/// # `POST /_matrix/client/r0/pushers/set`
///
/// Adds a pusher for the sender user.
///
/// - TODO: Handle `append`
#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/pushers/set", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn set_pushers_route(
    db: DatabaseGuard,
    body: Ruma<set_pusher::Request>,
) -> ConduitResult<set_pusher::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let pusher = body.pusher.clone();

    db.pusher.set_pusher(sender_user, pusher)?;

    db.flush()?;

    Ok(set_pusher::Response::default().into())
}
