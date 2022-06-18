use crate::{
    client_server,
    database::DatabaseGuard,
    pdu::{EventHash, PduBuilder, PduEvent},
    server_server, utils, Database, Error, Result, Ruma,
};
use ruma::{
    api::{
        client::{
            error::ErrorKind,
            membership::{
                ban_user, forget_room, get_member_events, invite_user, join_room_by_id,
                join_room_by_id_or_alias, joined_members, joined_rooms, kick_user, leave_room,
                unban_user, IncomingThirdPartySigned,
            },
        },
        federation::{self, membership::create_invite},
    },
    events::{
        room::{
            create::RoomCreateEventContent,
            member::{MembershipState, RoomMemberEventContent},
        },
        RoomEventType, StateEventType,
    },
    serde::{to_canonical_value, Base64, CanonicalJsonObject, CanonicalJsonValue},
    state_res::{self, RoomVersion},
    uint, EventId, RoomId, RoomVersionId, ServerName, UserId,
};
use serde_json::value::{to_raw_value, RawValue as RawJsonValue};
use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap},
    iter,
    sync::{Arc, RwLock},
    time::{Duration, Instant},
};
use tracing::{debug, error, warn};

/// # `POST /_matrix/client/r0/rooms/{roomId}/join`
///
/// Tries to join the sender user into a room.
///
/// - If the server knowns about this room: creates the join event and does auth rules locally
/// - If the server does not know about the room: asks other servers over federation
pub async fn join_room_by_id_route(
    db: DatabaseGuard,
    body: Ruma<join_room_by_id::v3::IncomingRequest>,
) -> Result<join_room_by_id::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut servers = Vec::new(); // There is no body.server_name for /roomId/join
    servers.extend(
        db.rooms
            .invite_state(sender_user, &body.room_id)?
            .unwrap_or_default()
            .iter()
            .filter_map(|event| serde_json::from_str(event.json().get()).ok())
            .filter_map(|event: serde_json::Value| event.get("sender").cloned())
            .filter_map(|sender| sender.as_str().map(|s| s.to_owned()))
            .filter_map(|sender| UserId::parse(sender).ok())
            .map(|user| user.server_name().to_owned()),
    );

    servers.push(body.room_id.server_name().to_owned());

    let ret = join_room_by_id_helper(
        &db,
        body.sender_user.as_deref(),
        &body.room_id,
        &servers,
        body.third_party_signed.as_ref(),
    )
    .await;

    db.flush()?;

    ret
}

/// # `POST /_matrix/client/r0/join/{roomIdOrAlias}`
///
/// Tries to join the sender user into a room.
///
/// - If the server knowns about this room: creates the join event and does auth rules locally
/// - If the server does not know about the room: asks other servers over federation
pub async fn join_room_by_id_or_alias_route(
    db: DatabaseGuard,
    body: Ruma<join_room_by_id_or_alias::v3::IncomingRequest>,
) -> Result<join_room_by_id_or_alias::v3::Response> {
    let sender_user = body.sender_user.as_deref().expect("user is authenticated");
    let body = body.body;

    let (servers, room_id) = match Box::<RoomId>::try_from(body.room_id_or_alias) {
        Ok(room_id) => {
            let mut servers = body.server_name.clone();
            servers.extend(
                db.rooms
                    .invite_state(sender_user, &room_id)?
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|event| serde_json::from_str(event.json().get()).ok())
                    .filter_map(|event: serde_json::Value| event.get("sender").cloned())
                    .filter_map(|sender| sender.as_str().map(|s| s.to_owned()))
                    .filter_map(|sender| UserId::parse(sender).ok())
                    .map(|user| user.server_name().to_owned()),
            );

            servers.push(room_id.server_name().to_owned());
            (servers, room_id)
        }
        Err(room_alias) => {
            let response = client_server::get_alias_helper(&db, &room_alias).await?;

            (response.servers.into_iter().collect(), response.room_id)
        }
    };

    let join_room_response = join_room_by_id_helper(
        &db,
        Some(sender_user),
        &room_id,
        &servers,
        body.third_party_signed.as_ref(),
    )
    .await?;

    db.flush()?;

    Ok(join_room_by_id_or_alias::v3::Response {
        room_id: join_room_response.room_id,
    })
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/leave`
///
/// Tries to leave the sender user from a room.
///
/// - This should always work if the user is currently joined.
pub async fn leave_room_route(
    db: DatabaseGuard,
    body: Ruma<leave_room::v3::IncomingRequest>,
) -> Result<leave_room::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.rooms.leave_room(sender_user, &body.room_id, &db).await?;

    db.flush()?;

    Ok(leave_room::v3::Response::new())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/invite`
///
/// Tries to send an invite event into the room.
pub async fn invite_user_route(
    db: DatabaseGuard,
    body: Ruma<invite_user::v3::IncomingRequest>,
) -> Result<invite_user::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if let invite_user::v3::IncomingInvitationRecipient::UserId { user_id } = &body.recipient {
        invite_helper(sender_user, user_id, &body.room_id, &db, false).await?;
        db.flush()?;
        Ok(invite_user::v3::Response {})
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "User not found."))
    }
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/kick`
///
/// Tries to send a kick event into the room.
pub async fn kick_user_route(
    db: DatabaseGuard,
    body: Ruma<kick_user::v3::IncomingRequest>,
) -> Result<kick_user::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut event: RoomMemberEventContent = serde_json::from_str(
        db.rooms
            .room_state_get(
                &body.room_id,
                &StateEventType::RoomMember,
                &body.user_id.to_string(),
            )?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "Cannot kick member that's not in the room.",
            ))?
            .content
            .get(),
    )
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = MembershipState::Leave;
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
            event_type: RoomEventType::RoomMember,
            content: to_raw_value(&event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        sender_user,
        &body.room_id,
        &db,
        &state_lock,
    )?;

    drop(state_lock);

    db.flush()?;

    Ok(kick_user::v3::Response::new())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/ban`
///
/// Tries to send a ban event into the room.
pub async fn ban_user_route(
    db: DatabaseGuard,
    body: Ruma<ban_user::v3::IncomingRequest>,
) -> Result<ban_user::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    // TODO: reason

    let event = db
        .rooms
        .room_state_get(
            &body.room_id,
            &StateEventType::RoomMember,
            &body.user_id.to_string(),
        )?
        .map_or(
            Ok(RoomMemberEventContent {
                membership: MembershipState::Ban,
                displayname: db.users.displayname(&body.user_id)?,
                avatar_url: db.users.avatar_url(&body.user_id)?,
                is_direct: None,
                third_party_invite: None,
                blurhash: db.users.blurhash(&body.user_id)?,
                reason: None,
                join_authorized_via_users_server: None,
            }),
            |event| {
                serde_json::from_str(event.content.get())
                    .map(|event: RoomMemberEventContent| RoomMemberEventContent {
                        membership: MembershipState::Ban,
                        ..event
                    })
                    .map_err(|_| Error::bad_database("Invalid member event in database."))
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
            event_type: RoomEventType::RoomMember,
            content: to_raw_value(&event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        sender_user,
        &body.room_id,
        &db,
        &state_lock,
    )?;

    drop(state_lock);

    db.flush()?;

    Ok(ban_user::v3::Response::new())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/unban`
///
/// Tries to send an unban event into the room.
pub async fn unban_user_route(
    db: DatabaseGuard,
    body: Ruma<unban_user::v3::IncomingRequest>,
) -> Result<unban_user::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut event: RoomMemberEventContent = serde_json::from_str(
        db.rooms
            .room_state_get(
                &body.room_id,
                &StateEventType::RoomMember,
                &body.user_id.to_string(),
            )?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "Cannot unban a user who is not banned.",
            ))?
            .content
            .get(),
    )
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = MembershipState::Leave;

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
            event_type: RoomEventType::RoomMember,
            content: to_raw_value(&event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        sender_user,
        &body.room_id,
        &db,
        &state_lock,
    )?;

    drop(state_lock);

    db.flush()?;

    Ok(unban_user::v3::Response::new())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/forget`
///
/// Forgets about a room.
///
/// - If the sender user currently left the room: Stops sender user from receiving information about the room
///
/// Note: Other devices of the user have no way of knowing the room was forgotten, so this has to
/// be called from every device
pub async fn forget_room_route(
    db: DatabaseGuard,
    body: Ruma<forget_room::v3::IncomingRequest>,
) -> Result<forget_room::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.rooms.forget(&body.room_id, sender_user)?;

    db.flush()?;

    Ok(forget_room::v3::Response::new())
}

/// # `POST /_matrix/client/r0/joined_rooms`
///
/// Lists all rooms the user has joined.
pub async fn joined_rooms_route(
    db: DatabaseGuard,
    body: Ruma<joined_rooms::v3::Request>,
) -> Result<joined_rooms::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    Ok(joined_rooms::v3::Response {
        joined_rooms: db
            .rooms
            .rooms_joined(sender_user)
            .filter_map(|r| r.ok())
            .collect(),
    })
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/members`
///
/// Lists all joined users in a room (TODO: at a specific point in time, with a specific membership).
///
/// - Only works if the user is currently joined
pub async fn get_member_events_route(
    db: DatabaseGuard,
    body: Ruma<get_member_events::v3::IncomingRequest>,
) -> Result<get_member_events::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    // TODO: check history visibility?
    if !db.rooms.is_joined(sender_user, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    Ok(get_member_events::v3::Response {
        chunk: db
            .rooms
            .room_state_full(&body.room_id).await?
            .iter()
            .filter(|(key, _)| key.0 == StateEventType::RoomMember)
            .map(|(_, pdu)| pdu.to_member_event().into())
            .collect(),
    })
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/joined_members`
///
/// Lists all members of a room.
///
/// - The sender user must be in the room
/// - TODO: An appservice just needs a puppet joined
pub async fn joined_members_route(
    db: DatabaseGuard,
    body: Ruma<joined_members::v3::IncomingRequest>,
) -> Result<joined_members::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(sender_user, &body.room_id)? {
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
            joined_members::v3::RoomMember {
                display_name,
                avatar_url,
            },
        );
    }

    Ok(joined_members::v3::Response { joined })
}

#[tracing::instrument(skip(db))]
async fn join_room_by_id_helper(
    db: &Database,
    sender_user: Option<&UserId>,
    room_id: &RoomId,
    servers: &[Box<ServerName>],
    _third_party_signed: Option<&IncomingThirdPartySigned>,
) -> Result<join_room_by_id::v3::Response> {
    let sender_user = sender_user.expect("user is authenticated");

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(room_id.to_owned())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    // Ask a remote server if we don't have this room
    if !db.rooms.exists(room_id)? {
        let mut make_join_response_and_server = Err(Error::BadServerResponse(
            "No server available to assist in joining.",
        ));

        for remote_server in servers {
            let make_join_response = db
                .sending
                .send_federation_request(
                    &db.globals,
                    remote_server,
                    federation::membership::prepare_join_event::v1::Request {
                        room_id,
                        user_id: sender_user,
                        ver: &db.globals.supported_room_versions(),
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
            Some(room_version) if db.rooms.is_supported_version(&db, &room_version) => room_version,
            _ => return Err(Error::BadServerResponse("Room version is not supported")),
        };

        let mut join_event_stub: CanonicalJsonObject =
            serde_json::from_str(make_join_response.event.get()).map_err(|_| {
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
            to_canonical_value(RoomMemberEventContent {
                membership: MembershipState::Join,
                displayname: db.users.displayname(sender_user)?,
                avatar_url: db.users.avatar_url(sender_user)?,
                is_direct: None,
                third_party_invite: None,
                blurhash: db.users.blurhash(sender_user)?,
                reason: None,
                join_authorized_via_users_server: None,
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
        let event_id = format!(
            "${}",
            ruma::signatures::reference_hash(&join_event_stub, &room_version)
                .expect("ruma can calculate reference hashes")
        );
        let event_id = <&EventId>::try_from(event_id.as_str())
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
                    event_id,
                    pdu: &PduEvent::convert_to_outgoing_federation_event(join_event.clone()),
                },
            )
            .await?;

        db.rooms.get_or_create_shortroomid(room_id, &db.globals)?;

        let parsed_pdu = PduEvent::from_id_val(event_id, join_event.clone())
            .map_err(|_| Error::BadServerResponse("Invalid join event PDU."))?;

        let mut state = HashMap::new();
        let pub_key_map = RwLock::new(BTreeMap::new());

        server_server::fetch_join_signing_keys(
            &send_join_response,
            &room_version,
            &pub_key_map,
            db,
        )
        .await?;

        for result in send_join_response
            .room_state
            .state
            .iter()
            .map(|pdu| validate_and_add_event_id(pdu, &room_version, &pub_key_map, db))
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
                let shortstatekey = db.rooms.get_or_create_shortstatekey(
                    &pdu.kind.to_string().into(),
                    state_key,
                    &db.globals,
                )?;
                state.insert(shortstatekey, pdu.event_id.clone());
            }
        }

        let incoming_shortstatekey = db.rooms.get_or_create_shortstatekey(
            &parsed_pdu.kind.to_string().into(),
            parsed_pdu
                .state_key
                .as_ref()
                .expect("Pdu is a membership state event"),
            &db.globals,
        )?;

        state.insert(incoming_shortstatekey, parsed_pdu.event_id.clone());

        let create_shortstatekey = db
            .rooms
            .get_shortstatekey(&StateEventType::RoomCreate, "")?
            .expect("Room exists");

        if state.get(&create_shortstatekey).is_none() {
            return Err(Error::BadServerResponse("State contained no create event."));
        }

        db.rooms.force_state(
            room_id,
            state
                .into_iter()
                .map(|(k, id)| db.rooms.compress_state_event(k, &id, &db.globals))
                .collect::<Result<_>>()?,
            db,
        )?;

        for result in send_join_response
            .room_state
            .auth_chain
            .iter()
            .map(|pdu| validate_and_add_event_id(pdu, &room_version, &pub_key_map, db))
        {
            let (event_id, value) = match result {
                Ok(t) => t,
                Err(_) => continue,
            };

            db.rooms.add_pdu_outlier(&event_id, &value)?;
        }

        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        let statehashid = db.rooms.append_to_state(&parsed_pdu, &db.globals)?;

        db.rooms.append_pdu(
            &parsed_pdu,
            join_event,
            iter::once(&*parsed_pdu.event_id),
            db,
        )?;

        // We set the room state after inserting the pdu, so that we never have a moment in time
        // where events in the current room state do not exist
        db.rooms.set_room_state(room_id, statehashid)?;
    } else {
        let event = RoomMemberEventContent {
            membership: MembershipState::Join,
            displayname: db.users.displayname(sender_user)?,
            avatar_url: db.users.avatar_url(sender_user)?,
            is_direct: None,
            third_party_invite: None,
            blurhash: db.users.blurhash(sender_user)?,
            reason: None,
            join_authorized_via_users_server: None,
        };

        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: RoomEventType::RoomMember,
                content: to_raw_value(&event).expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(sender_user.to_string()),
                redacts: None,
            },
            sender_user,
            room_id,
            db,
            &state_lock,
        )?;
    }

    drop(state_lock);

    db.flush()?;

    Ok(join_room_by_id::v3::Response::new(room_id.to_owned()))
}

fn validate_and_add_event_id(
    pdu: &RawJsonValue,
    room_version: &RoomVersionId,
    pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
    db: &Database,
) -> Result<(Box<EventId>, CanonicalJsonObject)> {
    let mut value: CanonicalJsonObject = serde_json::from_str(pdu.get()).map_err(|e| {
        error!("Invalid PDU in server response: {:?}: {:?}", pdu, e);
        Error::BadServerResponse("Invalid PDU in server response")
    })?;
    let event_id = EventId::parse(format!(
        "${}",
        ruma::signatures::reference_hash(&value, room_version)
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

pub(crate) async fn invite_helper<'a>(
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
                    .entry(room_id.to_owned())
                    .or_default(),
            );
            let state_lock = mutex_state.lock().await;

            let prev_events: Vec<_> = db
                .rooms
                .get_pdu_leaves(room_id)?
                .into_iter()
                .take(20)
                .collect();

            let create_event = db
                .rooms
                .room_state_get(room_id, &StateEventType::RoomCreate, "")?;

            let create_event_content: Option<RoomCreateEventContent> = create_event
                .as_ref()
                .map(|create_event| {
                    serde_json::from_str(create_event.content.get()).map_err(|e| {
                        warn!("Invalid create event: {}", e);
                        Error::bad_database("Invalid create event in db.")
                    })
                })
                .transpose()?;

            // If there was no create event yet, assume we are creating a room with the default
            // version right now
            let room_version_id = create_event_content
                .map_or(db.globals.default_room_version(), |create_event| {
                    create_event.room_version
                });
            let room_version =
                RoomVersion::new(&room_version_id).expect("room version is supported");

            let content = to_raw_value(&RoomMemberEventContent {
                avatar_url: None,
                displayname: None,
                is_direct: Some(is_direct),
                membership: MembershipState::Invite,
                third_party_invite: None,
                blurhash: None,
                reason: None,
                join_authorized_via_users_server: None,
            })
            .expect("member event is valid value");

            let state_key = user_id.to_string();
            let kind = StateEventType::RoomMember;

            let auth_events = db.rooms.get_auth_events(
                room_id,
                &kind.to_string().into(),
                sender_user,
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
                    to_raw_value(&prev_pdu.sender).expect("UserId is valid"),
                );
            }

            let pdu = PduEvent {
                event_id: ruma::event_id!("$thiswillbefilledinlater").into(),
                room_id: room_id.to_owned(),
                sender: sender_user.to_owned(),
                origin_server_ts: utils::millis_since_unix_epoch()
                    .try_into()
                    .expect("time is valid"),
                kind: kind.to_string().into(),
                content,
                state_key: Some(state_key),
                prev_events,
                depth,
                auth_events: auth_events
                    .iter()
                    .map(|(_, pdu)| pdu.event_id.clone())
                    .collect(),
                redacts: None,
                unsigned: if unsigned.is_empty() {
                    None
                } else {
                    Some(to_raw_value(&unsigned).expect("to_raw_value always works"))
                },
                hashes: EventHash {
                    sha256: "aaa".to_owned(),
                },
                signatures: None,
            };

            let auth_check = state_res::auth_check(
                &room_version,
                &pdu,
                None::<PduEvent>, // TODO: third_party_invite
                |k, s| auth_events.get(&(k.clone(), s.to_owned())),
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
        let expected_event_id = format!(
            "${}",
            ruma::signatures::reference_hash(&pdu_json, &room_version_id)
                .expect("ruma can calculate reference hashes")
        );
        let expected_event_id = <&EventId>::try_from(expected_event_id.as_str())
            .expect("ruma's reference hashes are valid event ids");

        let response = db
            .sending
            .send_federation_request(
                &db.globals,
                user_id.server_name(),
                create_invite::v2::Request {
                    room_id,
                    event_id: expected_event_id,
                    room_version: &room_version_id,
                    event: &PduEvent::convert_to_outgoing_federation_event(pdu_json.clone()),
                    invite_room_state: &invite_room_state,
                },
            )
            .await?;

        let pub_key_map = RwLock::new(BTreeMap::new());

        // We do not add the event_id field to the pdu here because of signature and hashes checks
        let (event_id, value) = match crate::pdu::gen_event_id_canonical_json(&response.event, &db)
        {
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

        let origin: Box<ServerName> = serde_json::from_value(
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
            room_id,
            value,
            true,
            db,
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

        let servers = db
            .rooms
            .room_servers(room_id)
            .filter_map(|r| r.ok())
            .filter(|server| &**server != db.globals.server_name());

        db.sending.send_pdu(servers, &pdu_id)?;

        return Ok(());
    }

    if !db.rooms.is_joined(sender_user, &room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    let mutex_state = Arc::clone(
        db.globals
            .roomid_mutex_state
            .write()
            .unwrap()
            .entry(room_id.to_owned())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: RoomEventType::RoomMember,
            content: to_raw_value(&RoomMemberEventContent {
                membership: MembershipState::Invite,
                displayname: db.users.displayname(user_id)?,
                avatar_url: db.users.avatar_url(user_id)?,
                is_direct: Some(is_direct),
                third_party_invite: None,
                blurhash: db.users.blurhash(user_id)?,
                reason: None,
                join_authorized_via_users_server: None,
            })
            .expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(user_id.to_string()),
            redacts: None,
        },
        sender_user,
        room_id,
        db,
        &state_lock,
    )?;

    drop(state_lock);

    Ok(())
}
