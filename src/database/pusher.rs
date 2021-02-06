use crate::{Database, Error, PduEvent, Result};
use log::{error, info, warn};
use ruma::{
    api::{
        client::r0::push::{Pusher, PusherKind},
        push_gateway::send_event_notification::{
            self,
            v1::{Device, Notification, NotificationCounts, NotificationPriority},
        },
        OutgoingRequest,
    },
    events::room::{
        member::{MemberEventContent, MembershipState},
        message::{MessageEventContent, TextMessageEventContent},
        power_levels::PowerLevelsEventContent,
    },
    events::EventType,
    push::{Action, PushCondition, PushFormat, Ruleset, Tweak},
    uint, UInt, UserId,
};

use std::{convert::TryFrom, fmt::Debug, time::Duration};

#[derive(Debug, Clone)]
pub struct PushData {
    /// UserId + pushkey -> Pusher
    pub(super) senderkey_pusher: sled::Tree,
}

impl PushData {
    pub fn new(db: &sled::Db) -> Result<Self> {
        Ok(Self {
            senderkey_pusher: db.open_tree("senderkey_pusher")?,
        })
    }

    pub fn set_pusher(&self, sender: &UserId, pusher: Pusher) -> Result<()> {
        let mut key = sender.as_bytes().to_vec();
        key.extend_from_slice(pusher.pushkey.as_bytes());

        // There are 2 kinds of pushers but the spec says: null deletes the pusher.
        if pusher.kind.is_none() {
            return self
                .senderkey_pusher
                .remove(key)
                .map(|_| ())
                .map_err(Into::into);
        }

        self.senderkey_pusher.insert(
            key,
            &*serde_json::to_string(&pusher).expect("Pusher is valid JSON string"),
        )?;

        Ok(())
    }

    pub fn get_pusher(&self, sender: &UserId) -> Result<Vec<Pusher>> {
        self.senderkey_pusher
            .scan_prefix(sender.as_bytes())
            .values()
            .map(|push: std::result::Result<sled::IVec, _>| {
                let push = push.map_err(|_| Error::bad_database("Invalid push bytes in db."))?;
                Ok(serde_json::from_slice(&*push)
                    .map_err(|_| Error::bad_database("Invalid Pusher in db."))?)
            })
            .collect()
    }
}

pub async fn send_request<T: OutgoingRequest>(
    globals: &crate::database::globals::Globals,
    destination: &str,
    request: T,
) -> Result<T::IncomingResponse>
where
    T: Debug,
{
    let destination = destination.replace("/_matrix/push/v1/notify", "");

    let http_request = request
        .try_into_http_request(&destination, Some(""))
        .map_err(|e| {
            warn!("Failed to find destination {}: {}", destination, e);
            Error::BadServerResponse("Invalid destination")
        })?;

    let mut reqwest_request = reqwest::Request::try_from(http_request)
        .expect("all http requests are valid reqwest requests");

    // TODO: we could keep this very short and let expo backoff do it's thing...
    *reqwest_request.timeout_mut() = Some(Duration::from_secs(5));

    let url = reqwest_request.url().clone();
    let reqwest_response = globals.reqwest_client().execute(reqwest_request).await;

    // Because reqwest::Response -> http::Response is complicated:
    match reqwest_response {
        Ok(mut reqwest_response) => {
            let status = reqwest_response.status();
            let mut http_response = http::Response::builder().status(status);
            let headers = http_response.headers_mut().unwrap();

            for (k, v) in reqwest_response.headers_mut().drain() {
                if let Some(key) = k {
                    headers.insert(key, v);
                }
            }

            let status = reqwest_response.status();

            let body = reqwest_response
                .bytes()
                .await
                .unwrap_or_else(|e| {
                    warn!("server error {}", e);
                    Vec::new().into()
                }) // TODO: handle timeout
                .into_iter()
                .collect::<Vec<_>>();

            if status != 200 {
                info!(
                    "Push gateway returned bad response {} {}\n{}\n{:?}",
                    destination,
                    status,
                    url,
                    crate::utils::string_from_bytes(&body)
                );
            }

            let response = T::IncomingResponse::try_from(
                http_response
                    .body(body)
                    .expect("reqwest body is valid http body"),
            );
            response.map_err(|_| {
                info!(
                    "Push gateway returned invalid response bytes {}\n{}",
                    destination, url
                );
                Error::BadServerResponse("Push gateway returned bad response.")
            })
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn send_push_notice(
    user: &UserId,
    unread: UInt,
    pushers: &[Pusher],
    ruleset: Ruleset,
    pdu: &PduEvent,
    db: &Database,
) -> Result<()> {
    for rule in ruleset.into_iter() {
        // TODO: can actions contain contradictory Actions
        if rule
            .actions
            .iter()
            .any(|act| matches!(act, ruma::push::Action::DontNotify))
            || !rule.enabled
        {
            continue;
        }

        match rule.rule_id.as_str() {
            ".m.rule.master" => {}
            ".m.rule.suppress_notices" => {
                if pdu.kind == EventType::RoomMessage
                    && pdu
                        .content
                        .get("msgtype")
                        .map_or(false, |ty| ty == "m.notice")
                {
                    let tweaks = rule
                        .actions
                        .iter()
                        .filter_map(|a| match a {
                            Action::SetTweak(tweak) => Some(tweak.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str()).await?;
                    break;
                }
            }
            ".m.rule.invite_for_me" => {
                if let EventType::RoomMember = &pdu.kind {
                    if pdu.state_key.as_deref() == Some(user.as_str())
                        && serde_json::from_value::<MemberEventContent>(pdu.content.clone())
                            .map_err(|_| Error::bad_database("PDU contained bad message content"))?
                            .membership
                            == MembershipState::Invite
                    {
                        let tweaks = rule
                            .actions
                            .iter()
                            .filter_map(|a| match a {
                                Action::SetTweak(tweak) => Some(tweak.clone()),
                                _ => None,
                            })
                            .collect::<Vec<_>>();
                        send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str())
                            .await?;
                        break;
                    }
                }
            }
            ".m.rule.member_event" => {
                if let EventType::RoomMember = &pdu.kind {
                    // TODO use this?
                    let _member = serde_json::from_value::<MemberEventContent>(pdu.content.clone())
                        .map_err(|_| Error::bad_database("PDU contained bad message content"))?;
                    if let Some(conditions) = rule.conditions {
                        if conditions.iter().any(|cond| match cond {
                            PushCondition::EventMatch { key, pattern } => {
                                let mut json =
                                    serde_json::to_value(pdu).expect("PDU is valid JSON");
                                for key in key.split('.') {
                                    json = json[key].clone();
                                }
                                // TODO: this is baddddd
                                json.to_string().contains(pattern)
                            }
                            _ => false,
                        }) {
                            let tweaks = rule
                                .actions
                                .iter()
                                .filter_map(|a| match a {
                                    Action::SetTweak(tweak) => Some(tweak.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str())
                                .await?;
                            break;
                        }
                    }
                }
            }
            ".m.rule.contains_display_name" => {
                if let EventType::RoomMessage = &pdu.kind {
                    let msg_content =
                        serde_json::from_value::<MessageEventContent>(pdu.content.clone())
                            .map_err(|_| {
                                Error::bad_database("PDU contained bad message content")
                            })?;
                    if let MessageEventContent::Text(TextMessageEventContent { body, .. }) =
                        &msg_content
                    {
                        if body.contains(user.localpart()) {
                            let tweaks = rule
                                .actions
                                .iter()
                                .filter_map(|a| match a {
                                    Action::SetTweak(tweak) => Some(tweak.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str())
                                .await?;
                            break;
                        }
                    }
                }
            }
            ".m.rule.tombstone" => {
                if pdu.kind == EventType::RoomTombstone && pdu.state_key.as_deref() == Some("") {
                    let tweaks = rule
                        .actions
                        .iter()
                        .filter_map(|a| match a {
                            Action::SetTweak(tweak) => Some(tweak.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str()).await?;
                    break;
                }
            }
            ".m.rule.roomnotif" => {
                if let EventType::RoomMessage = &pdu.kind {
                    let msg_content =
                        serde_json::from_value::<MessageEventContent>(pdu.content.clone())
                            .map_err(|_| {
                                Error::bad_database("PDU contained bad message content")
                            })?;
                    if let MessageEventContent::Text(TextMessageEventContent { body, .. }) =
                        &msg_content
                    {
                        let power_level_cmp = |pl: PowerLevelsEventContent| {
                            &pl.notifications.room
                                <= pl.users.get(&pdu.sender).unwrap_or(&ruma::int!(0))
                        };
                        let deserialize = |pl: PduEvent| {
                            serde_json::from_value::<PowerLevelsEventContent>(pl.content).ok()
                        };
                        if body.contains("@room")
                            && db
                                .rooms
                                .room_state_get(&pdu.room_id, &EventType::RoomPowerLevels, "")?
                                .map(|(_, pl)| pl)
                                .map(deserialize)
                                .flatten()
                                .map_or(false, power_level_cmp)
                        {
                            let tweaks = rule
                                .actions
                                .iter()
                                .filter_map(|a| match a {
                                    Action::SetTweak(tweak) => Some(tweak.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str())
                                .await?;
                            break;
                        }
                    }
                }
            }
            ".m.rule.contains_user_name" => {
                if let EventType::RoomMessage = &pdu.kind {
                    let msg_content =
                        serde_json::from_value::<MessageEventContent>(pdu.content.clone())
                            .map_err(|_| {
                                Error::bad_database("PDU contained bad message content")
                            })?;
                    if let MessageEventContent::Text(TextMessageEventContent { body, .. }) =
                        &msg_content
                    {
                        if body.contains(user.localpart()) {
                            let tweaks = rule
                                .actions
                                .iter()
                                .filter_map(|a| match a {
                                    Action::SetTweak(tweak) => Some(tweak.clone()),
                                    _ => None,
                                })
                                .collect::<Vec<_>>();
                            send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str())
                                .await?;
                            break;
                        }
                    }
                }
            }
            ".m.rule.call" => {
                if pdu.kind == EventType::CallInvite {
                    let tweaks = rule
                        .actions
                        .iter()
                        .filter_map(|a| match a {
                            Action::SetTweak(tweak) => Some(tweak.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str()).await?;
                    break;
                }
            }
            ".m.rule.encrypted_room_one_to_one" => {
                if db.rooms.room_members(&pdu.room_id).count() == 2
                    && pdu.kind == EventType::RoomEncrypted
                {
                    let tweaks = rule
                        .actions
                        .iter()
                        .filter_map(|a| match a {
                            Action::SetTweak(tweak) => Some(tweak.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str()).await?;
                    break;
                }
            }
            ".m.rule.room_one_to_one" => {
                if db.rooms.room_members(&pdu.room_id).count() == 2
                    && pdu.kind == EventType::RoomMessage
                {
                    let tweaks = rule
                        .actions
                        .iter()
                        .filter_map(|a| match a {
                            Action::SetTweak(tweak) => Some(tweak.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str()).await?;
                    break;
                }
            }
            ".m.rule.message" => {
                if pdu.kind == EventType::RoomMessage {
                    let tweaks = rule
                        .actions
                        .iter()
                        .filter_map(|a| match a {
                            Action::SetTweak(tweak) => Some(tweak.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str()).await?;
                    break;
                }
            }
            ".m.rule.encrypted" => {
                if pdu.kind == EventType::RoomEncrypted {
                    let tweaks = rule
                        .actions
                        .iter()
                        .filter_map(|a| match a {
                            Action::SetTweak(tweak) => Some(tweak.clone()),
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    send_notice(unread, pushers, tweaks, pdu, db, rule.rule_id.as_str()).await?;
                    break;
                }
            }
            _ => {}
        }
    }

    Ok(())
}

async fn send_notice(
    unread: UInt,
    pushers: &[Pusher],
    tweaks: Vec<Tweak>,
    event: &PduEvent,
    db: &Database,
    name: &str,
) -> Result<()> {
    let (http, _emails): (Vec<&Pusher>, _) = pushers
        .iter()
        .partition(|pusher| pusher.kind == Some(PusherKind::Http));

    // TODO:
    // Two problems with this
    // 1. if "event_id_only" is the only format kind it seems we should never add more info
    // 2. can pusher/devices have conflicting formats
    for pusher in http {
        let event_id_only = pusher.data.format == Some(PushFormat::EventIdOnly);
        let url = if let Some(url) = pusher.data.url.as_ref() {
            url
        } else {
            error!("Http Pusher must have URL specified.");
            continue;
        };

        let mut device = Device::new(pusher.app_id.clone(), pusher.pushkey.clone());
        let mut data_minus_url = pusher.data.clone();
        // The url must be stripped off according to spec
        data_minus_url.url = None;
        device.data = Some(data_minus_url);

        // Tweaks are only added if the format is NOT event_id_only
        if !event_id_only {
            device.tweaks = tweaks.clone();
        }

        let d = &[device];
        let mut notifi = Notification::new(d);

        notifi.prio = NotificationPriority::Low;
        notifi.event_id = Some(&event.event_id);
        notifi.room_id = Some(&event.room_id);
        // TODO: missed calls
        notifi.counts = NotificationCounts::new(unread, uint!(0));

        if event.kind == EventType::RoomEncrypted
            || tweaks
                .iter()
                .any(|t| matches!(t, Tweak::Highlight(true) | Tweak::Sound(_)))
        {
            notifi.prio = NotificationPriority::High
        }

        if event_id_only {
            error!("SEND PUSH NOTICE `{}`", name);
        // send_request(
        //     &db.globals,
        //     &url,
        //     send_event_notification::v1::Request::new(notifi),
        // )
        // .await?;
        } else {
            notifi.sender = Some(&event.sender);
            notifi.event_type = Some(&event.kind);
            notifi.content = serde_json::value::to_raw_value(&event.content).ok();

            if event.kind == EventType::RoomMember {
                notifi.user_is_target = event.state_key.as_deref() == Some(event.sender.as_str());
            }

            let user_name = db.users.displayname(&event.sender)?;
            notifi.sender_display_name = user_name.as_deref();
            let room_name = db
                .rooms
                .room_state_get(&event.room_id, &EventType::RoomName, "")?
                .map(|(_, pdu)| match pdu.content.get("name") {
                    Some(serde_json::Value::String(s)) => Some(s.to_string()),
                    _ => None,
                })
                .flatten();
            notifi.room_name = room_name.as_deref();

            error!("SEND PUSH NOTICE Full `{}`", name);
            // send_request(
            //     &db.globals,
            //     &url,
            //     send_event_notification::v1::Request::new(notifi),
            // )
            // .await?;
        }
    }

    // TODO: email
    // for email in emails {}

    Ok(())
}
