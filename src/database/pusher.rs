use crate::{Database, Error, PduEvent, Result};
use bytes::BytesMut;
use log::{error, info, warn};
use ruma::{
    api::{
        client::r0::push::{get_pushers, set_pusher, PusherKind},
        push_gateway::send_event_notification::{
            self,
            v1::{Device, Notification, NotificationCounts, NotificationPriority},
        },
        IncomingResponse, OutgoingRequest, SendAccessToken,
    },
    events::{room::power_levels::PowerLevelsEventContent, EventType},
    push::{Action, PushConditionRoomCtx, PushFormat, Ruleset, Tweak},
    uint, UInt, UserId,
};

use std::{convert::TryFrom, fmt::Debug, mem, sync::Arc};

use super::abstraction::Tree;

pub struct PushData {
    /// UserId + pushkey -> Pusher
    pub(super) senderkey_pusher: Arc<dyn Tree>,
}

impl PushData {
    pub fn set_pusher(&self, sender: &UserId, pusher: set_pusher::Pusher) -> Result<()> {
        let mut key = sender.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(pusher.pushkey.as_bytes());

        // There are 2 kinds of pushers but the spec says: null deletes the pusher.
        if pusher.kind.is_none() {
            return self
                .senderkey_pusher
                .remove(&key)
                .map(|_| ())
                .map_err(Into::into);
        }

        self.senderkey_pusher.insert(
            &key,
            &serde_json::to_vec(&pusher).expect("Pusher is valid JSON value"),
        )?;

        Ok(())
    }

    pub fn get_pusher(&self, senderkey: &[u8]) -> Result<Option<get_pushers::Pusher>> {
        self.senderkey_pusher
            .get(senderkey)?
            .map(|push| {
                serde_json::from_slice(&*push)
                    .map_err(|_| Error::bad_database("Invalid Pusher in db."))
            })
            .transpose()
    }

    pub fn get_pushers(&self, sender: &UserId) -> Result<Vec<get_pushers::Pusher>> {
        let mut prefix = sender.as_bytes().to_vec();
        prefix.push(0xff);

        self.senderkey_pusher
            .scan_prefix(prefix)
            .map(|(_, push)| {
                serde_json::from_slice(&*push)
                    .map_err(|_| Error::bad_database("Invalid Pusher in db."))
            })
            .collect()
    }

    pub fn get_pusher_senderkeys<'a>(
        &'a self,
        sender: &UserId,
    ) -> impl Iterator<Item = Vec<u8>> + 'a {
        let mut prefix = sender.as_bytes().to_vec();
        prefix.push(0xff);

        self.senderkey_pusher.scan_prefix(prefix).map(|(k, _)| k)
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
        .try_into_http_request::<BytesMut>(&destination, SendAccessToken::IfRequired(""))
        .map_err(|e| {
            warn!("Failed to find destination {}: {}", destination, e);
            Error::BadServerResponse("Invalid destination")
        })?
        .map(|body| body.freeze());

    let reqwest_request = reqwest::Request::try_from(http_request)
        .expect("all http requests are valid reqwest requests");

    // TODO: we could keep this very short and let expo backoff do it's thing...
    //*reqwest_request.timeout_mut() = Some(Duration::from_secs(5));

    let url = reqwest_request.url().clone();
    let response = globals.reqwest_client().execute(reqwest_request).await;

    match response {
        Ok(mut response) => {
            // reqwest::Response -> http::Response conversion
            let status = response.status();
            let mut http_response_builder = http::Response::builder()
                .status(status)
                .version(response.version());
            mem::swap(
                response.headers_mut(),
                http_response_builder
                    .headers_mut()
                    .expect("http::response::Builder is usable"),
            );

            let body = response.bytes().await.unwrap_or_else(|e| {
                warn!("server error {}", e);
                Vec::new().into()
            }); // TODO: handle timeout

            if status != 200 {
                info!(
                    "Push gateway returned bad response {} {}\n{}\n{:?}",
                    destination,
                    status,
                    url,
                    crate::utils::string_from_bytes(&body)
                );
            }

            let response = T::IncomingResponse::try_from_http_response(
                http_response_builder
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
    pusher: &get_pushers::Pusher,
    ruleset: Ruleset,
    pdu: &PduEvent,
    db: &Database,
) -> Result<()> {
    let mut notify = None;
    let mut tweaks = Vec::new();

    for action in get_actions(user, &ruleset, pdu, db)? {
        let n = match action {
            Action::DontNotify => false,
            // TODO: Implement proper support for coalesce
            Action::Notify | Action::Coalesce => true,
            Action::SetTweak(tweak) => {
                tweaks.push(tweak.clone());
                continue;
            }
        };

        if notify.is_some() {
            return Err(Error::bad_database(
                r#"Malformed pushrule contains more than one of these actions: ["dont_notify", "notify", "coalesce"]"#,
            ));
        }

        notify = Some(n);
    }

    if notify == Some(true) {
        send_notice(unread, pusher, tweaks, pdu, db).await?;
    }
    // Else the event triggered no actions

    Ok(())
}

pub fn get_actions<'a>(
    user: &UserId,
    ruleset: &'a Ruleset,
    pdu: &PduEvent,
    db: &Database,
) -> Result<&'a [Action]> {
    let power_levels: PowerLevelsEventContent = db
        .rooms
        .room_state_get(&pdu.room_id, &EventType::RoomPowerLevels, "")?
        .map(|ev| {
            serde_json::from_value(ev.content.clone())
                .map_err(|_| Error::bad_database("invalid m.room.power_levels event"))
        })
        .transpose()?
        .unwrap_or_default();

    let ctx = PushConditionRoomCtx {
        room_id: pdu.room_id.clone(),
        member_count: 10_u32.into(), // TODO: get member count efficiently
        user_display_name: db
            .users
            .displayname(&user)?
            .unwrap_or_else(|| user.localpart().to_owned()),
        users_power_levels: power_levels.users,
        default_power_level: power_levels.users_default,
        notification_power_levels: power_levels.notifications,
    };

    Ok(ruleset.get_actions(&pdu.to_sync_room_event(), &ctx))
}

async fn send_notice(
    unread: UInt,
    pusher: &get_pushers::Pusher,
    tweaks: Vec<Tweak>,
    event: &PduEvent,
    db: &Database,
) -> Result<()> {
    // TODO: email
    if pusher.kind == PusherKind::Email {
        return Ok(());
    }

    // TODO:
    // Two problems with this
    // 1. if "event_id_only" is the only format kind it seems we should never add more info
    // 2. can pusher/devices have conflicting formats
    let event_id_only = pusher.data.format == Some(PushFormat::EventIdOnly);
    let url = if let Some(url) = &pusher.data.url {
        url
    } else {
        error!("Http Pusher must have URL specified.");
        return Ok(());
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
        send_request(
            &db.globals,
            &url,
            send_event_notification::v1::Request::new(notifi),
        )
        .await?;
    } else {
        notifi.sender = Some(&event.sender);
        notifi.event_type = Some(&event.kind);
        let content = serde_json::value::to_raw_value(&event.content).ok();
        notifi.content = content.as_deref();

        if event.kind == EventType::RoomMember {
            notifi.user_is_target = event.state_key.as_deref() == Some(event.sender.as_str());
        }

        let user_name = db.users.displayname(&event.sender)?;
        notifi.sender_display_name = user_name.as_deref();
        let room_name = db
            .rooms
            .room_state_get(&event.room_id, &EventType::RoomName, "")?
            .map(|pdu| match pdu.content.get("name") {
                Some(serde_json::Value::String(s)) => Some(s.to_string()),
                _ => None,
            })
            .flatten();
        notifi.room_name = room_name.as_deref();

        send_request(
            &db.globals,
            &url,
            send_event_notification::v1::Request::new(notifi),
        )
        .await?;
    }

    // TODO: email

    Ok(())
}
