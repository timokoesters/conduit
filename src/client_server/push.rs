use super::State;
use crate::{ConduitResult, Database, Error, Ruma};
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
    push::{
        ConditionalPushRuleInit, ContentPushRule, OverridePushRule, PatternedPushRuleInit,
        RoomPushRule, SenderPushRule, SimplePushRuleInit, UnderridePushRule,
    },
};

#[cfg(feature = "conduit_bin")]
use rocket::{delete, get, post, put};

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushrules", data = "<body>")
)]
pub async fn get_pushrules_all_route(
    db: State<'_, Database>,
    body: Ruma<get_pushrules_all::Request>,
) -> ConduitResult<get_pushrules_all::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, &sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    Ok(get_pushrules_all::Response {
        global: event.content.global,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushrules/<_>/<_>/<_>", data = "<body>")
)]
pub async fn get_pushrule_route(
    db: State<'_, Database>,
    body: Ruma<get_pushrule::Request<'_>>,
) -> ConduitResult<get_pushrule::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, &sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = event.content.global;
    let rule = match body.kind {
        RuleKind::Override => global
            .override_
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.clone().into()),
        RuleKind::Underride => global
            .underride
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.clone().into()),
        RuleKind::Sender => global
            .sender
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.clone().into()),
        RuleKind::Room => global
            .room
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.clone().into()),
        RuleKind::Content => global
            .content
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.clone().into()),
        RuleKind::_Custom(_) => None,
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

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/pushrules/<_>/<_>/<_>", data = "<body>")
)]
pub async fn set_pushrule_route(
    db: State<'_, Database>,
    body: Ruma<set_pushrule::Request<'_>>,
) -> ConduitResult<set_pushrule::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if body.scope != "global" {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Scopes other than 'global' are not supported.",
        ));
    }

    let mut event = db
        .account_data
        .get::<push_rules::PushRulesEvent>(None, &sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    match body.kind {
        RuleKind::Override => {
            if let Some(rule) = global
                .override_
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.override_.remove(&rule);
            }

            global.override_.insert(OverridePushRule(
                ConditionalPushRuleInit {
                    actions: body.actions.clone(),
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id.clone(),
                    conditions: body.conditions.clone(),
                }
                .into(),
            ));
        }
        RuleKind::Underride => {
            if let Some(rule) = global
                .underride
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.underride.remove(&rule);
            }

            global.underride.insert(UnderridePushRule(
                ConditionalPushRuleInit {
                    actions: body.actions.clone(),
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id.clone(),
                    conditions: body.conditions.clone(),
                }
                .into(),
            ));
        }
        RuleKind::Sender => {
            if let Some(rule) = global
                .sender
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.sender.remove(&rule);
            }

            global.sender.insert(SenderPushRule(
                SimplePushRuleInit {
                    actions: body.actions.clone(),
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id.clone(),
                }
                .into(),
            ));
        }
        RuleKind::Room => {
            if let Some(rule) = global
                .room
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.room.remove(&rule);
            }

            global.room.insert(RoomPushRule(
                SimplePushRuleInit {
                    actions: body.actions.clone(),
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id.clone(),
                }
                .into(),
            ));
        }
        RuleKind::Content => {
            if let Some(rule) = global
                .content
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.content.remove(&rule);
            }

            global.content.insert(ContentPushRule(
                PatternedPushRuleInit {
                    actions: body.actions.clone(),
                    default: false,
                    enabled: true,
                    rule_id: body.rule_id.clone(),
                    pattern: body.pattern.clone().unwrap_or_default(),
                }
                .into(),
            ));
        }
        RuleKind::_Custom(_) => {}
    }

    db.account_data.update(
        None,
        &sender_user,
        EventType::PushRules,
        &event,
        &db.globals,
    )?;

    db.flush().await?;

    Ok(set_pushrule::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushrules/<_>/<_>/<_>/actions", data = "<body>")
)]
pub async fn get_pushrule_actions_route(
    db: State<'_, Database>,
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
        .get::<push_rules::PushRulesEvent>(None, &sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    let actions = match body.kind {
        RuleKind::Override => global
            .override_
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.actions.clone()),
        RuleKind::Underride => global
            .underride
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.actions.clone()),
        RuleKind::Sender => global
            .sender
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.actions.clone()),
        RuleKind::Room => global
            .room
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.actions.clone()),
        RuleKind::Content => global
            .content
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map(|rule| rule.0.actions.clone()),
        RuleKind::_Custom(_) => None,
    };

    db.flush().await?;

    Ok(get_pushrule_actions::Response {
        actions: actions.unwrap_or_default(),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/pushrules/<_>/<_>/<_>/actions", data = "<body>")
)]
pub async fn set_pushrule_actions_route(
    db: State<'_, Database>,
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
        .get::<push_rules::PushRulesEvent>(None, &sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    match body.kind {
        RuleKind::Override => {
            if let Some(mut rule) = global
                .override_
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.override_.remove(&rule);
                rule.0.actions = body.actions.clone();
                global.override_.insert(rule);
            }
        }
        RuleKind::Underride => {
            if let Some(mut rule) = global
                .underride
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.underride.remove(&rule);
                rule.0.actions = body.actions.clone();
                global.underride.insert(rule);
            }
        }
        RuleKind::Sender => {
            if let Some(mut rule) = global
                .sender
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.sender.remove(&rule);
                rule.0.actions = body.actions.clone();
                global.sender.insert(rule);
            }
        }
        RuleKind::Room => {
            if let Some(mut rule) = global
                .room
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.room.remove(&rule);
                rule.0.actions = body.actions.clone();
                global.room.insert(rule);
            }
        }
        RuleKind::Content => {
            if let Some(mut rule) = global
                .content
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.content.remove(&rule);
                rule.0.actions = body.actions.clone();
                global.content.insert(rule);
            }
        }
        RuleKind::_Custom(_) => {}
    };

    db.account_data.update(
        None,
        &sender_user,
        EventType::PushRules,
        &event,
        &db.globals,
    )?;

    db.flush().await?;

    Ok(set_pushrule_actions::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushrules/<_>/<_>/<_>/enabled", data = "<body>")
)]
pub async fn get_pushrule_enabled_route(
    db: State<'_, Database>,
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
        .get::<push_rules::PushRulesEvent>(None, &sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    let enabled = match body.kind {
        RuleKind::Override => global
            .override_
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map_or(false, |rule| rule.0.enabled),
        RuleKind::Underride => global
            .underride
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map_or(false, |rule| rule.0.enabled),
        RuleKind::Sender => global
            .sender
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map_or(false, |rule| rule.0.enabled),
        RuleKind::Room => global
            .room
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map_or(false, |rule| rule.0.enabled),
        RuleKind::Content => global
            .content
            .iter()
            .find(|rule| rule.0.rule_id == body.rule_id)
            .map_or(false, |rule| rule.0.enabled),
        RuleKind::_Custom(_) => false,
    };

    db.flush().await?;

    Ok(get_pushrule_enabled::Response { enabled }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/pushrules/<_>/<_>/<_>/enabled", data = "<body>")
)]
pub async fn set_pushrule_enabled_route(
    db: State<'_, Database>,
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
        .get::<push_rules::PushRulesEvent>(None, &sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    match body.kind {
        RuleKind::Override => {
            if let Some(mut rule) = global
                .override_
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.override_.remove(&rule);
                rule.0.enabled = body.enabled;
                global.override_.insert(rule);
            }
        }
        RuleKind::Underride => {
            if let Some(mut rule) = global
                .underride
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.underride.remove(&rule);
                rule.0.enabled = body.enabled;
                global.underride.insert(rule);
            }
        }
        RuleKind::Sender => {
            if let Some(mut rule) = global
                .sender
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.sender.remove(&rule);
                rule.0.enabled = body.enabled;
                global.sender.insert(rule);
            }
        }
        RuleKind::Room => {
            if let Some(mut rule) = global
                .room
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.room.remove(&rule);
                rule.0.enabled = body.enabled;
                global.room.insert(rule);
            }
        }
        RuleKind::Content => {
            if let Some(mut rule) = global
                .content
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.content.remove(&rule);
                rule.0.enabled = body.enabled;
                global.content.insert(rule);
            }
        }
        RuleKind::_Custom(_) => {}
    }

    db.account_data.update(
        None,
        &sender_user,
        EventType::PushRules,
        &event,
        &db.globals,
    )?;

    db.flush().await?;

    Ok(set_pushrule_enabled::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    delete("/_matrix/client/r0/pushrules/<_>/<_>/<_>", data = "<body>")
)]
pub async fn delete_pushrule_route(
    db: State<'_, Database>,
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
        .get::<push_rules::PushRulesEvent>(None, &sender_user, EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?;

    let global = &mut event.content.global;
    match body.kind {
        RuleKind::Override => {
            if let Some(rule) = global
                .override_
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.override_.remove(&rule);
            }
        }
        RuleKind::Underride => {
            if let Some(rule) = global
                .underride
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.underride.remove(&rule);
            }
        }
        RuleKind::Sender => {
            if let Some(rule) = global
                .sender
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.sender.remove(&rule);
            }
        }
        RuleKind::Room => {
            if let Some(rule) = global
                .room
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.room.remove(&rule);
            }
        }
        RuleKind::Content => {
            if let Some(rule) = global
                .content
                .iter()
                .find(|rule| rule.0.rule_id == body.rule_id)
                .cloned()
            {
                global.content.remove(&rule);
            }
        }
        RuleKind::_Custom(_) => {}
    }

    db.account_data.update(
        None,
        &sender_user,
        EventType::PushRules,
        &event,
        &db.globals,
    )?;

    db.flush().await?;

    Ok(delete_pushrule::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/pushers", data = "<body>")
)]
pub async fn get_pushers_route(
    db: State<'_, Database>,
    body: Ruma<get_pushers::Request>,
) -> ConduitResult<get_pushers::Response> {
    let sender = body.sender_user.as_ref().expect("authenticated endpoint");

    Ok(get_pushers::Response {
        pushers: db.pusher.get_pusher(sender)?,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/pushers/set", data = "<body>")
)]
pub async fn set_pushers_route(
    db: State<'_, Database>,
    body: Ruma<set_pusher::Request>,
) -> ConduitResult<set_pusher::Response> {
    let sender = body.sender_user.as_ref().expect("authenticated endpoint");
    let pusher = body.pusher.clone();

    db.pusher.set_pusher(sender, pusher)?;

    db.flush().await?;

    Ok(set_pusher::Response::default().into())
}
