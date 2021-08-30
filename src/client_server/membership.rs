use crate::{
    client_server,
    database::DatabaseGuard,
    pdu::{PduBuilder, PduEvent},
    server_server, utils, ConduitResult, Database, Error, Result, Ruma,
};
use member::{MemberEventContent, MembershipState};
use rocket::futures;
use ruma::{
    api::{
        client::{
            error::ErrorKind,
            r0::membership::{
                ban_user, forget_room, get_member_events, invite_user, join_room_by_id,
                join_room_by_id_or_alias, joined_members, joined_rooms, kick_user, leave_room,
                unban_user, IncomingThirdPartySigned,
            },
        },
        federation::{self, membership::create_invite},
    },
    events::{
        pdu::Pdu,
        room::{create::CreateEventContent, member},
        EventType,
    },
    serde::{to_canonical_value, CanonicalJsonObject, CanonicalJsonValue, Raw},
    state_res::{self, RoomVersion},
    uint, EventId, RoomId, RoomVersionId, ServerName, UserId,
};
use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tracing::{debug, error, warn};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/join", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn join_room_by_id_route(
    db: DatabaseGuard,
    body: Ruma<join_room_by_id::Request<'_>>,
) -> ConduitResult<join_room_by_id::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut servers = db
        .rooms
        .invite_state(&sender_user, &body.room_id)?
        .unwrap_or_default()
        .iter()
        .filter_map(|event| {
            serde_json::from_str::<serde_json::Value>(&event.json().to_string()).ok()
        })
        .filter_map(|event| event.get("sender").cloned())
        .filter_map(|sender| sender.as_str().map(|s| s.to_owned()))
        .filter_map(|sender| UserId::try_from(sender).ok())
        .map(|user| user.server_name().to_owned())
        .collect::<HashSet<_>>();

    servers.insert(body.room_id.server_name().to_owned());

    let ret = join_room_by_id_helper(
        &db,
        body.sender_user.as_ref(),
        &body.room_id,
        &servers,
        body.third_party_signed.as_ref(),
    )
    .await;

    db.flush()?;

    ret
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/join/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn join_room_by_id_or_alias_route(
    db: DatabaseGuard,
    body: Ruma<join_room_by_id_or_alias::Request<'_>>,
) -> ConduitResult<join_room_by_id_or_alias::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let (servers, room_id) = match RoomId::try_from(body.room_id_or_alias.clone()) {
        Ok(room_id) => {
            let mut servers = db
                .rooms
                .invite_state(&sender_user, &room_id)?
                .unwrap_or_default()
                .iter()
                .filter_map(|event| {
                    serde_json::from_str::<serde_json::Value>(&event.json().to_string()).ok()
                })
                .filter_map(|event| event.get("sender").cloned())
                .filter_map(|sender| sender.as_str().map(|s| s.to_owned()))
                .filter_map(|sender| UserId::try_from(sender).ok())
                .map(|user| user.server_name().to_owned())
                .collect::<HashSet<_>>();

            servers.insert(room_id.server_name().to_owned());
            (servers, room_id)
        }
        Err(room_alias) => {
            let response = client_server::get_alias_helper(&db, &room_alias).await?;

            (response.0.servers.into_iter().collect(), response.0.room_id)
        }
    };

    let join_room_response = join_room_by_id_helper(
        &db,
        body.sender_user.as_ref(),
        &room_id,
        &servers,
        body.third_party_signed.as_ref(),
    )
    .await?;

    db.flush()?;

    Ok(join_room_by_id_or_alias::Response {
        room_id: join_room_response.0.room_id,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/leave", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn leave_room_route(
    db: DatabaseGuard,
    body: Ruma<leave_room::Request<'_>>,
) -> ConduitResult<leave_room::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.rooms.leave_room(sender_user, &body.room_id, &db).await?;

    db.flush()?;

    Ok(leave_room::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/invite", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn invite_user_route(
    db: DatabaseGuard,
    body: Ruma<invite_user::Request<'_>>,
) -> ConduitResult<invite_user::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if let invite_user::IncomingInvitationRecipient::UserId { user_id } = &body.recipient {
        invite_helper(sender_user, user_id, &body.room_id, &db, false).await?;
        db.flush()?;
        Ok(invite_user::Response {}.into())
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "User not found."))
    }
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/kick", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn kick_user_route(
    db: DatabaseGuard,
    body: Ruma<kick_user::Request<'_>>,
) -> ConduitResult<kick_user::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut event = serde_json::from_value::<Raw<ruma::events::room::member::MemberEventContent>>(
        db.rooms
            .room_state_get(
                &body.room_id,
                &EventType::RoomMember,
                &body.user_id.to_string(),
            )?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "Cannot kick member that's not in the room.",
            ))?
            .content
            .clone(),
    )
    .expect("Raw::from_value always works")
    .deserialize()
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = ruma::events::room::member::MembershipState::Leave;
    // TODO: reason

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &sender_user,
        &body.room_id,
        &db,
        &state_lock,
    )?;

    drop(state_lock);

    db.flush()?;

    Ok(kick_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/ban", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn ban_user_route(
    db: DatabaseGuard,
    body: Ruma<ban_user::Request<'_>>,
) -> ConduitResult<ban_user::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    // TODO: reason

    let event = db
        .rooms
        .room_state_get(
            &body.room_id,
            &EventType::RoomMember,
            &body.user_id.to_string(),
        )?
        .map_or(
            Ok::<_, Error>(member::MemberEventContent {
                membership: member::MembershipState::Ban,
                displayname: db.users.displayname(&body.user_id)?,
                avatar_url: db.users.avatar_url(&body.user_id)?,
                is_direct: None,
                third_party_invite: None,
                blurhash: db.users.blurhash(&body.user_id)?,
                reason: None,
            }),
            |event| {
                let mut event = serde_json::from_value::<Raw<member::MemberEventContent>>(
                    event.content.clone(),
                )
                .expect("Raw::from_value always works")
                .deserialize()
                .map_err(|_| Error::bad_database("Invalid member event in database."))?;
                event.membership = ruma::events::room::member::MembershipState::Ban;
                Ok(event)
            },
        )?;

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &sender_user,
        &body.room_id,
        &db,
        &state_lock,
    )?;

    drop(state_lock);

    db.flush()?;

    Ok(ban_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/unban", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn unban_user_route(
    db: DatabaseGuard,
    body: Ruma<unban_user::Request<'_>>,
) -> ConduitResult<unban_user::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut event = serde_json::from_value::<Raw<ruma::events::room::member::MemberEventContent>>(
        db.rooms
            .room_state_get(
                &body.room_id,
                &EventType::RoomMember,
                &body.user_id.to_string(),
            )?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "Cannot unban a user who is not banned.",
            ))?
            .content
            .clone(),
    )
    .expect("from_value::<Raw<..>> can never fail")
    .deserialize()
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = ruma::events::room::member::MembershipState::Leave;

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &sender_user,
        &body.room_id,
        &db,
        &state_lock,
    )?;

    drop(state_lock);

    db.flush()?;

    Ok(unban_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/forget", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn forget_room_route(
    db: DatabaseGuard,
    body: Ruma<forget_room::Request<'_>>,
) -> ConduitResult<forget_room::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.rooms.forget(&body.room_id, &sender_user)?;

    db.flush()?;

    Ok(forget_room::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/joined_rooms", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn joined_rooms_route(
    db: DatabaseGuard,
    body: Ruma<joined_rooms::Request>,
) -> ConduitResult<joined_rooms::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    Ok(joined_rooms::Response {
        joined_rooms: db
            .rooms
            .rooms_joined(&sender_user)
            .filter_map(|r| r.ok())
            .collect(),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/members", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_member_events_route(
    db: DatabaseGuard,
    body: Ruma<get_member_events::Request<'_>>,
) -> ConduitResult<get_member_events::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(sender_user, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    Ok(get_member_events::Response {
        chunk: db
            .rooms
            .room_state_full(&body.room_id)?
            .iter()
            .filter(|(key, _)| key.0 == EventType::RoomMember)
            .map(|(_, pdu)| pdu.to_member_event())
            .collect(),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/joined_members", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn joined_members_route(
    db: DatabaseGuard,
    body: Ruma<joined_members::Request<'_>>,
) -> ConduitResult<joined_members::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !db
        .rooms
        .is_joined(&sender_user, &body.room_id)
        .unwrap_or(false)
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You aren't a member of the room.",
        ));
    }

    let mut joined = BTreeMap::new();
    for user_id in db.rooms.room_members(&body.room_id).filter_map(|r| r.ok()) {
        let display_name = db.users.displayname(&user_id)?;
        let avatar_url = db.users.avatar_url(&user_id)?;

        joined.insert(
            user_id,
            joined_members::RoomMember {
                display_name,
                avatar_url,
            },
        );
    }

    Ok(joined_members::Response { joined }.into())
}

#[tracing::instrument(skip(db))]
async fn join_room_by_id_helper(
    db: &Database,
    sender_user: Option<&UserId>,
    room_id: &RoomId,
    servers: &HashSet<Box<ServerName>>,
    _third_party_signed: Option<&IncomingThirdPartySigned>,
) -> ConduitResult<join_room_by_id::Response> {
    let sender_user = sender_user.expect("user is authenticated");

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    // Ask a remote server if we don't have this room
    if !db.rooms.exists(&room_id)? && room_id.server_name() != db.globals.server_name() {
        let mut make_join_response_and_server = Err(Error::BadServerResponse(
            "No server available to assist in joining.",
        ));

        for remote_server in servers {
            let make_join_response = db
                .sending
                .send_federation_request(
                    &db.globals,
                    remote_server,
                    federation::membership::create_join_event_template::v1::Request {
                        room_id,
                        user_id: sender_user,
                        ver: &[RoomVersionId::Version5, RoomVersionId::Version6],
                    },
                )
                .await;

            make_join_response_and_server = make_join_response.map(|r| (r, remote_server));

            if make_join_response_and_server.is_ok() {
                break;
            }
        }

        let (make_join_response, remote_server) = make_join_response_and_server?;

        let room_version = match make_join_response.room_version {
            Some(room_version)
                if room_version == RoomVersionId::Version5
                    || room_version == RoomVersionId::Version6 =>
            {
                room_version
            }
            _ => return Err(Error::BadServerResponse("Room version is not supported")),
        };

        let mut join_event_stub =
            serde_json::from_str::<CanonicalJsonObject>(make_join_response.event.json().get())
                .map_err(|_| {
                    Error::BadServerResponse("Invalid make_join event json received from server.")
                })?;

        // TODO: Is origin needed?
        join_event_stub.insert(
            "origin".to_owned(),
            CanonicalJsonValue::String(db.globals.server_name().as_str().to_owned()),
        );
        join_event_stub.insert(
            "origin_server_ts".to_owned(),
            CanonicalJsonValue::Integer(
                utils::millis_since_unix_epoch()
                    .try_into()
                    .expect("Timestamp is valid js_int value"),
            ),
        );
        join_event_stub.insert(
            "content".to_owned(),
            to_canonical_value(member::MemberEventContent {
                membership: member::MembershipState::Join,
                displayname: db.users.displayname(&sender_user)?,
                avatar_url: db.users.avatar_url(&sender_user)?,
                is_direct: None,
                third_party_invite: None,
                blurhash: db.users.blurhash(&sender_user)?,
                reason: None,
            })
            .expect("event is valid, we just created it"),
        );

        // We don't leave the event id in the pdu because that's only allowed in v1 or v2 rooms
        join_event_stub.remove("event_id");

        // In order to create a compatible ref hash (EventID) the `hashes` field needs to be present
        ruma::signatures::hash_and_sign_event(
            db.globals.server_name().as_str(),
            db.globals.keypair(),
            &mut join_event_stub,
            &room_version,
        )
        .expect("event is valid, we just created it");

        // Generate event id
        let event_id = EventId::try_from(&*format!(
            "${}",
            ruma::signatures::reference_hash(&join_event_stub, &room_version)
                .expect("ruma can calculate reference hashes")
        ))
        .expect("ruma's reference hashes are valid event ids");

        // Add event_id back
        join_event_stub.insert(
            "event_id".to_owned(),
            CanonicalJsonValue::String(event_id.as_str().to_owned()),
        );

        // It has enough fields to be called a proper event now
        let join_event = join_event_stub;

        let send_join_response = db
            .sending
            .send_federation_request(
                &db.globals,
                remote_server,
                federation::membership::create_join_event::v2::Request {
                    room_id,
                    event_id: &event_id,
                    pdu: PduEvent::convert_to_outgoing_federation_event(join_event.clone()),
                },
            )
            .await?;

        db.rooms.get_or_create_shortroomid(&room_id, &db.globals)?;

        let pdu = PduEvent::from_id_val(&event_id, join_event.clone())
            .map_err(|_| Error::BadServerResponse("Invalid join event PDU."))?;

        let mut state = HashMap::new();
        let pub_key_map = RwLock::new(BTreeMap::new());

        for result in futures::future::join_all(
            send_join_response
                .room_state
                .state
                .iter()
                .map(|pdu| validate_and_add_event_id(pdu, &room_version, &pub_key_map, &db)),
        )
        .await
        {
            let (event_id, value) = match result {
                Ok(t) => t,
                Err(_) => continue,
            };

            let pdu = PduEvent::from_id_val(&event_id, value.clone()).map_err(|e| {
                warn!("{:?}: {}", value, e);
                Error::BadServerResponse("Invalid PDU in send_join response.")
            })?;

            db.rooms.add_pdu_outlier(&event_id, &value)?;
            if let Some(state_key) = &pdu.state_key {
                let shortstatekey =
                    db.rooms
                        .get_or_create_shortstatekey(&pdu.kind, state_key, &db.globals)?;
                state.insert(shortstatekey, pdu.event_id.clone());
            }
        }

        let incoming_shortstatekey = db.rooms.get_or_create_shortstatekey(
            &pdu.kind,
            pdu.state_key
                .as_ref()
                .expect("Pdu is a membership state event"),
            &db.globals,
        )?;

        state.insert(incoming_shortstatekey, pdu.event_id.clone());

        let create_shortstatekey = db
            .rooms
            .get_shortstatekey(&EventType::RoomCreate, "")?
            .expect("Room exists");

        if state.get(&create_shortstatekey).is_none() {
            return Err(Error::BadServerResponse("State contained no create event."));
        }

        db.rooms.force_state(
            room_id,
            state
                .into_iter()
                .map(|(k, id)| db.rooms.compress_state_event(k, &id, &db.globals))
                .collect::<Result<HashSet<_>>>()?,
            &db,
        )?;

        for result in futures::future::join_all(
            send_join_response
                .room_state
                .auth_chain
                .iter()
                .map(|pdu| validate_and_add_event_id(pdu, &room_version, &pub_key_map, &db)),
        )
        .await
        {
            let (event_id, value) = match result {
                Ok(t) => t,
                Err(_) => continue,
            };

            db.rooms.add_pdu_outlier(&event_id, &value)?;
        }

        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        let statehashid = db.rooms.append_to_state(&pdu, &db.globals)?;

        db.rooms.append_pdu(
            &pdu,
            utils::to_canonical_object(&pdu).expect("Pdu is valid canonical object"),
            &[pdu.event_id.clone()],
            db,
        )?;

        // We set the room state after inserting the pdu, so that we never have a moment in time
        // where events in the current room state do not exist
        db.rooms.set_room_state(&room_id, statehashid)?;
    } else {
        let event = member::MemberEventContent {
            membership: member::MembershipState::Join,
            displayname: db.users.displayname(&sender_user)?,
            avatar_url: db.users.avatar_url(&sender_user)?,
            is_direct: None,
            third_party_invite: None,
            blurhash: db.users.blurhash(&sender_user)?,
            reason: None,
        };

        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(event).expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(sender_user.to_string()),
                redacts: None,
            },
            &sender_user,
            &room_id,
            &db,
            &state_lock,
        )?;
    }

    drop(state_lock);

    db.flush()?;

    Ok(join_room_by_id::Response::new(room_id.clone()).into())
}

async fn validate_and_add_event_id(
    pdu: &Raw<Pdu>,
    room_version: &RoomVersionId,
    pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, String>>>,
    db: &Database,
) -> Result<(EventId, CanonicalJsonObject)> {
    let mut value = serde_json::from_str::<CanonicalJsonObject>(pdu.json().get()).map_err(|e| {
        error!("Invalid PDU in server response: {:?}: {:?}", pdu, e);
        Error::BadServerResponse("Invalid PDU in server response")
    })?;
    let event_id = EventId::try_from(&*format!(
        "${}",
        ruma::signatures::reference_hash(&value, &room_version)
            .expect("ruma can calculate reference hashes")
    ))
    .expect("ruma's reference hashes are valid event ids");

    let back_off = |id| match db.globals.bad_event_ratelimiter.write().unwrap().entry(id) {
        Entry::Vacant(e) => {
            e.insert((Instant::now(), 1));
        }
        Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
    };

    if let Some((time, tries)) = db
        .globals
        .bad_event_ratelimiter
        .read()
        .unwrap()
        .get(&event_id)
    {
        // Exponential backoff
        let mut min_elapsed_duration = Duration::from_secs(30) * (*tries) * (*tries);
        if min_elapsed_duration > Duration::from_secs(60 * 60 * 24) {
            min_elapsed_duration = Duration::from_secs(60 * 60 * 24);
        }

        if time.elapsed() < min_elapsed_duration {
            debug!("Backing off from {}", event_id);
            return Err(Error::BadServerResponse("bad event, still backing off"));
        }
    }

    server_server::fetch_required_signing_keys(&value, pub_key_map, db).await?;
    if let Err(e) = ruma::signatures::verify_event(
        &*pub_key_map
            .read()
            .map_err(|_| Error::bad_database("RwLock is poisoned."))?,
        &value,
        room_version,
    ) {
        warn!("Event {} failed verification {:?} {}", event_id, pdu, e);
        back_off(event_id);
        return Err(Error::BadServerResponse("Event failed verification."));
    }

    value.insert(
        "event_id".to_owned(),
        CanonicalJsonValue::String(event_id.as_str().to_owned()),
    );

    Ok((event_id, value))
}

pub async fn invite_helper<'a>(
    sender_user: &UserId,
    user_id: &UserId,
    room_id: &RoomId,
    db: &Database,
    is_direct: bool,
) -> Result<()> {
    if user_id.server_name() != db.globals.server_name() {
        let (room_version_id, pdu_json, invite_room_state) = {
            let mutex_state = Arc::clone(
                db.globals
                    .roomid_mutex_state
                    .write()
                    .unwrap()
                    .entry(room_id.clone())
                    .or_default(),
            );
            let state_lock = mutex_state.lock().await;

            let prev_events = db
                .rooms
                .get_pdu_leaves(room_id)?
                .into_iter()
                .take(20)
                .collect::<Vec<_>>();

            let create_event = db
                .rooms
                .room_state_get(room_id, &EventType::RoomCreate, "")?;

            let create_event_content = create_event
                .as_ref()
                .map(|create_event| {
                    serde_json::from_value::<Raw<CreateEventContent>>(create_event.content.clone())
                        .expect("Raw::from_value always works.")
                        .deserialize()
                        .map_err(|e| {
                            warn!("Invalid create event: {}", e);
                            Error::bad_database("Invalid create event in db.")
                        })
                })
                .transpose()?;

            let create_prev_event = if prev_events.len() == 1
                && Some(&prev_events[0]) == create_event.as_ref().map(|c| &c.event_id)
            {
                create_event
            } else {
                None
            };

            // If there was no create event yet, assume we are creating a version 6 room right now
            let room_version_id = create_event_content
                .map_or(RoomVersionId::Version6, |create_event| {
                    create_event.room_version
                });
            let room_version =
                RoomVersion::new(&room_version_id).expect("room version is supported");

            let content = serde_json::to_value(MemberEventContent {
                avatar_url: None,
                displayname: None,
                is_direct: Some(is_direct),
                membership: MembershipState::Invite,
                third_party_invite: None,
                blurhash: None,
                reason: None,
            })
            .expect("member event is valid value");

            let state_key = user_id.to_string();
            let kind = EventType::RoomMember;

            let auth_events = db.rooms.get_auth_events(
                room_id,
                &kind,
                &sender_user,
                Some(&state_key),
                &content,
            )?;

            // Our depth is the maximum depth of prev_events + 1
            let depth = prev_events
                .iter()
                .filter_map(|event_id| Some(db.rooms.get_pdu(event_id).ok()??.depth))
                .max()
                .unwrap_or_else(|| uint!(0))
                + uint!(1);

            let mut unsigned = BTreeMap::new();

            if let Some(prev_pdu) = db.rooms.room_state_get(room_id, &kind, &state_key)? {
                unsigned.insert("prev_content".to_owned(), prev_pdu.content.clone());
                unsigned.insert(
                    "prev_sender".to_owned(),
                    serde_json::to_value(&prev_pdu.sender).expect("UserId::to_value always works"),
                );
            }

            let pdu = PduEvent {
                event_id: ruma::event_id!("$thiswillbefilledinlater"),
                room_id: room_id.clone(),
                sender: sender_user.clone(),
                origin_server_ts: utils::millis_since_unix_epoch()
                    .try_into()
                    .expect("time is valid"),
                kind,
                content,
                state_key: Some(state_key),
                prev_events,
                depth,
                auth_events: auth_events
                    .iter()
                    .map(|(_, pdu)| pdu.event_id.clone())
                    .collect(),
                redacts: None,
                unsigned,
                hashes: ruma::events::pdu::EventHash {
                    sha256: "aaa".to_owned(),
                },
                signatures: BTreeMap::new(),
            };

            let auth_check = state_res::auth_check(
                &room_version,
                &Arc::new(pdu.clone()),
                create_prev_event,
                None, // TODO: third_party_invite
                |k, s| auth_events.get(&(k.clone(), s.to_owned())).map(Arc::clone),
            )
            .map_err(|e| {
                error!("{:?}", e);
                Error::bad_database("Auth check failed.")
            })?;

            if !auth_check {
                return Err(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "Event is not authorized.",
                ));
            }

            // Hash and sign
            let mut pdu_json =
                utils::to_canonical_object(&pdu).expect("event is valid, we just created it");

            pdu_json.remove("event_id");

            // Add origin because synapse likes that (and it's required in the spec)
            pdu_json.insert(
                "origin".to_owned(),
                to_canonical_value(db.globals.server_name())
                    .expect("server name is a valid CanonicalJsonValue"),
            );

            ruma::signatures::hash_and_sign_event(
                db.globals.server_name().as_str(),
                db.globals.keypair(),
                &mut pdu_json,
                &room_version_id,
            )
            .expect("event is valid, we just created it");

            let invite_room_state = db.rooms.calculate_invite_state(&pdu)?;

            drop(state_lock);

            (room_version_id, pdu_json, invite_room_state)
        };

        // Generate event id
        let expected_event_id = EventId::try_from(&*format!(
            "${}",
            ruma::signatures::reference_hash(&pdu_json, &room_version_id)
                .expect("ruma can calculate reference hashes")
        ))
        .expect("ruma's reference hashes are valid event ids");

        let response = db
            .sending
            .send_federation_request(
                &db.globals,
                user_id.server_name(),
                create_invite::v2::Request {
                    room_id: room_id.clone(),
                    event_id: expected_event_id.clone(),
                    room_version: room_version_id,
                    event: PduEvent::convert_to_outgoing_federation_event(pdu_json.clone()),
                    invite_room_state,
                },
            )
            .await?;

        let pub_key_map = RwLock::new(BTreeMap::new());

        // We do not add the event_id field to the pdu here because of signature and hashes checks
        let (event_id, value) = match crate::pdu::gen_event_id_canonical_json(&response.event) {
            Ok(t) => t,
            Err(_) => {
                // Event could not be converted to canonical json
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Could not convert event to canonical json.",
                ));
            }
        };

        if expected_event_id != event_id {
            warn!("Server {} changed invite event, that's not allowed in the spec: ours: {:?}, theirs: {:?}", user_id.server_name(), pdu_json, value);
        }

        let origin = serde_json::from_value::<Box<ServerName>>(
            serde_json::to_value(value.get("origin").ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event needs an origin field.",
            ))?)
            .expect("CanonicalJson is valid json value"),
        )
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Origin field is invalid."))?;

        let pdu_id = server_server::handle_incoming_pdu(
            &origin,
            &event_id,
            &room_id,
            value,
            true,
            &db,
            &pub_key_map,
        )
        .await
        .map_err(|_| {
            Error::BadRequest(
                ErrorKind::InvalidParam,
                "Error while handling incoming PDU.",
            )
        })?
        .ok_or(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Could not accept incoming PDU as timeline event.",
        ))?;

        for server in db
            .rooms
            .room_servers(room_id)
            .filter_map(|r| r.ok())
            .filter(|server| &**server != db.globals.server_name())
        {
            db.sending.send_pdu(&server, &pdu_id)?;
        }

        return Ok(());
    }

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(member::MemberEventContent {
                membership: member::MembershipState::Invite,
                displayname: db.users.displayname(&user_id)?,
                avatar_url: db.users.avatar_url(&user_id)?,
                is_direct: Some(is_direct),
                third_party_invite: None,
                blurhash: db.users.blurhash(&user_id)?,
                reason: None,
            })
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(user_id.to_string()),
            redacts: None,
        },
        &sender_user,
        room_id,
        &db,
        &state_lock,
    )?;

    drop(state_lock);

    Ok(())
}
