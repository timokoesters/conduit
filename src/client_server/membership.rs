use crate::{
    client_server,
    database::DatabaseGuard,
    pdu::{PduBuilder, PduEvent},
    server_server, utils, ConduitResult, Database, Error, Result, Ruma,
};
use member::{MemberEventContent, MembershipState};
use rayon::prelude::*;
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
        federation::{
            self,
            membership::{create_invite, create_join_event},
        },
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
use serde_json::value::RawValue;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::{TryFrom, TryInto},
    sync::{Arc, RwLock},
};
use tracing::{error, warn};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

/// # `POST /_matrix/client/r0/rooms/{roomId}/join`
///
/// Tries to join the sender user into a room.
///
/// - If the server knowns about this room: creates the join event and does auth rules locally
/// - If the server does not know about the room: asks other servers over federation
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
        .invite_state(sender_user, &body.room_id)?
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

/// # `POST /_matrix/client/r0/join/{roomIdOrAlias}`
///
/// Tries to join the sender user into a room.
///
/// - If the server knowns about this room: creates the join event and does auth rules locally
/// - If the server does not know about the room: asks other servers over federation
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
                .invite_state(sender_user, &room_id)?
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

/// # `POST /_matrix/client/r0/rooms/{roomId}/leave`
///
/// Tries to leave the sender user from a room.
///
/// - This should always work if the user is currently joined.
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

/// # `POST /_matrix/client/r0/rooms/{roomId}/invite`
///
/// Tries to send an invite event into the room.
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

/// # `POST /_matrix/client/r0/rooms/{roomId}/kick`
///
/// Tries to send a kick event into the room.
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

    let mut event = serde_json::from_str::<Raw<ruma::events::room::member::MemberEventContent>>(
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
            .get(),
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
            content: RawValue::from_string(
                serde_json::to_string(&event).expect("event is valid, we just created it"),
            )
            .expect("string is valid"),
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

    Ok(kick_user::Response::new().into())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/ban`
///
/// Tries to send a ban event into the room.
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
                let mut event =
                    serde_json::from_str::<member::MemberEventContent>(event.content.get())
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
            content: RawValue::from_string(
                serde_json::to_string(&event).expect("event is valid, we just created it"),
            )
            .expect("string is valid"),
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

    Ok(ban_user::Response::new().into())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/unban`
///
/// Tries to send an unban event into the room.
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

    let mut event = serde_json::from_str::<ruma::events::room::member::MemberEventContent>(
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
            .get(),
    )
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
            content: RawValue::from_string(
                serde_json::to_string(&event).expect("event is valid, we just created it"),
            )
            .expect("string is valid"),
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

    Ok(unban_user::Response::new().into())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/forget`
///
/// Forgets about a room.
///
/// - If the sender user currently left the room: Stops sender user from receiving information about the room
///
/// Note: Other devices of the user have no way of knowing the room was forgotten, so this has to
/// be called from every device
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

    db.rooms.forget(&body.room_id, sender_user)?;

    db.flush()?;

    Ok(forget_room::Response::new().into())
}

/// # `POST /_matrix/client/r0/joined_rooms`
///
/// Lists all rooms the user has joined.
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
            .rooms_joined(sender_user)
            .filter_map(|r| r.ok())
            .collect(),
    }
    .into())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/members`
///
/// Lists all joined users in a room (TODO: at a specific point in time, with a specific membership).
///
/// - Only works if the user is currently joined
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

    // TODO: check history visibility?
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

/// # `POST /_matrix/client/r0/rooms/{roomId}/joined_members`
///
/// Lists all members of a room.
///
/// - The sender user must be in the room
/// - TODO: An appservice just needs a puppet joined
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
    if !db.rooms.exists(room_id)? && room_id.server_name() != db.globals.server_name() {
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
            warn!("Make join done");

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
                displayname: db.users.displayname(sender_user)?,
                avatar_url: db.users.avatar_url(sender_user)?,
                is_direct: None,
                third_party_invite: None,
                blurhash: db.users.blurhash(sender_user)?,
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

        warn!("Send join done");

        db.rooms.get_or_create_shortroomid(room_id, &db.globals)?;

        let pdu = PduEvent::from_id_val(&event_id, join_event.clone())
            .map_err(|_| Error::BadServerResponse("Invalid join event PDU."))?;

        let pub_key_map = Arc::new(RwLock::new(BTreeMap::new()));
        let missing_servers = Arc::new(RwLock::new(BTreeMap::new()));

        let create_join_event::RoomState {
            state: mut room_state_state,
            auth_chain: mut room_state_auth_chain,
        } = send_join_response.room_state;

        let create_shortstatekey = db
            .rooms
            .get_shortstatekey(&EventType::RoomCreate, "")?
            .expect("Room exists");

        let mut saw_create_event = false;

        warn!("Parsing send join response state");
        const CHUNK_SIZE: usize = 500;
        let mut parsed_state = room_state_state
            .par_chunks_mut(CHUNK_SIZE)
            .filter_map(|pdus| {
                let mut r = HashMap::with_capacity(CHUNK_SIZE);
                for pdu in pdus {
                    let (id, value) = get_event_id(&pdu, &room_version).ok()?;
                    r.insert(id, value);
                }

                let mut missing_servers = missing_servers.write().unwrap();
                let mut pub_key_map = pub_key_map.write().unwrap();
                for (_, value) in &r {
                    server_server::get_server_keys_from_cache(
                        &value,
                        &mut missing_servers,
                        &mut pub_key_map,
                        &db,
                    )
                    .ok()?;
                }

                Some(r)
            })
            .collect::<Vec<_>>();

        warn!("Parsing send join response auth chain");
        let mut parsed_chain = room_state_auth_chain
            .par_chunks_mut(CHUNK_SIZE)
            .filter_map(|pdus| {
                let mut r = HashMap::with_capacity(CHUNK_SIZE);
                for pdu in pdus {
                    let (id, value) = get_event_id(&pdu, &room_version).ok()?;
                    r.insert(id, value);
                }

                let mut missing_servers = missing_servers.write().unwrap();
                let mut pub_key_map = pub_key_map.write().unwrap();
                for (_, value) in &r {
                    server_server::get_server_keys_from_cache(
                        &value,
                        &mut missing_servers,
                        &mut pub_key_map,
                        &db,
                    )
                    .ok()?;
                }

                Some(r)
            })
            .collect::<Vec<_>>();

        warn!("Fetching send join signing keys");
        server_server::fetch_join_signing_keys(missing_servers, &pub_key_map, db).await?;

        warn!("Validating state");
        parsed_state.par_iter_mut().for_each(|chunk| {
            let mut bad_events = Vec::new();
            for (event_id, value) in chunk.iter_mut() {
                if let Err(e) = ruma::signatures::verify_event(
                    &*pub_key_map.read().unwrap(),
                    &value,
                    &room_version,
                ) {
                    warn!("Event {} failed verification {:?} {}", event_id, value, e);
                    bad_events.push(event_id.clone());
                    continue;
                }

                value.insert(
                    "event_id".to_owned(),
                    CanonicalJsonValue::String(event_id.as_str().to_owned()),
                );
            }
            for id in bad_events {
                chunk.remove(&id);
            }
        });

        warn!("Inserting state");
        db.rooms
            .add_pdu_outlier_batch(&mut parsed_state.iter().flatten())?;

        warn!("Compressing state");
        let state = parsed_state
            .iter()
            .flatten()
            .map(|(event_id, value)| {
                let kind = if let Some(s) = value.get("type").and_then(|s| s.as_str()) {
                    s
                } else {
                    warn!("Event {} has no type: {:?}", event_id, value);
                    return Ok(None);
                };

                if let Some(state_key) = value.get("state_key").and_then(|s| s.as_str()) {
                    let shortstatekey = db.rooms.get_or_create_shortstatekey(
                        &EventType::from(kind),
                        state_key,
                        &db.globals,
                    )?;

                    if shortstatekey == create_shortstatekey {
                        saw_create_event = true;
                    }

                    Ok(Some(db.rooms.compress_state_event(
                        shortstatekey,
                        &event_id,
                        &db.globals,
                    )?))
                } else {
                    Ok(None)
                }
            })
            .filter_map(|r| r.transpose())
            .collect::<Result<HashSet<_>>>()?;

        if !saw_create_event {
            return Err(Error::BadServerResponse("State contained no create event."));
        }

        warn!("Validating chain");
        parsed_chain.par_iter_mut().for_each(|chunk| {
            let mut bad_events = Vec::new();
            for (event_id, value) in chunk.iter_mut() {
                if let Err(e) = ruma::signatures::verify_event(
                    &*pub_key_map.read().unwrap(),
                    &value,
                    &room_version,
                ) {
                    warn!("Event {} failed verification {:?} {}", event_id, value, e);
                    bad_events.push(event_id.clone());
                    continue;
                }

                value.insert(
                    "event_id".to_owned(),
                    CanonicalJsonValue::String(event_id.as_str().to_owned()),
                );
            }
            for id in bad_events {
                chunk.remove(&id);
            }
        });

        warn!("Inserting chain");
        db.rooms
            .add_pdu_outlier_batch(&mut parsed_chain.iter().flatten())?;

        warn!("Forcing state of room");
        db.rooms.force_state_new(
            room_id,
            state,
            &mut parsed_state.iter().flat_map(|m| m.values()),
            db,
        )?;

        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        warn!("Appending join event to state");
        let statehashid = db.rooms.append_to_state(&pdu, &db.globals)?;

        warn!("Adding join event to db");
        db.rooms.append_pdu(
            &pdu,
            utils::to_canonical_object(&pdu).expect("Pdu is valid canonical object"),
            &[pdu.event_id.clone()],
            db,
        )?;

        warn!("Updating room state to join event");
        // We set the room state after inserting the pdu, so that we never have a moment in time
        // where events in the current room state do not exist
        db.rooms.set_room_state(room_id, statehashid)?;
    } else {
        let event = member::MemberEventContent {
            membership: member::MembershipState::Join,
            displayname: db.users.displayname(sender_user)?,
            avatar_url: db.users.avatar_url(sender_user)?,
            is_direct: None,
            third_party_invite: None,
            blurhash: db.users.blurhash(sender_user)?,
            reason: None,
        };

        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: RawValue::from_string(
                    serde_json::to_string(&event).expect("event is valid, we just created it"),
                )
                .expect("string is valid"),
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

    Ok(join_room_by_id::Response::new(room_id.clone()).into())
}

fn get_event_id(
    pdu: &Raw<Pdu>,
    room_version: &RoomVersionId,
) -> Result<(EventId, CanonicalJsonObject)> {
    let value = serde_json::from_str::<CanonicalJsonObject>(pdu.json().get()).map_err(|e| {
        warn!("Invalid PDU in server response: {:?}: {:?}", pdu, e);
        Error::BadServerResponse("Invalid PDU in server response")
    })?;

    let event_id = EventId::try_from(&*format!(
        "${}",
        ruma::signatures::reference_hash(&value, room_version)
            .expect("ruma can calculate reference hashes")
    ))
    .expect("ruma's reference hashes are valid event ids");

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
                    serde_json::from_str::<Raw<CreateEventContent>>(create_event.content.get())
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

            let content = RawValue::from_string(
                serde_json::to_string(&MemberEventContent {
                    avatar_url: None,
                    displayname: None,
                    is_direct: Some(is_direct),
                    membership: MembershipState::Invite,
                    third_party_invite: None,
                    blurhash: None,
                    reason: None,
                })
                .expect("member event is valid value"),
            )
            .expect("string is valid");

            let state_key = user_id.to_string();
            let kind = EventType::RoomMember;

            let auth_events = db.rooms.get_auth_events(
                room_id,
                &kind,
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
                    serde_json::from_str(prev_pdu.sender.as_str()).expect("UserId is valid string"),
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
                parsed_content: RwLock::new(None),
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
                    Some(
                        RawValue::from_string(
                            serde_json::to_string(&unsigned).expect("to_string always works"),
                        )
                        .expect("string is valid"),
                    )
                },
                hashes: ruma::events::pdu::EventHash {
                    sha256: "aaa".to_owned(),
                },
                signatures: None,
            };

            let auth_check = state_res::auth_check(
                &room_version,
                &pdu,
                create_prev_event,
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
            content: RawValue::from_string(
                serde_json::to_string(&member::MemberEventContent {
                    membership: member::MembershipState::Invite,
                    displayname: db.users.displayname(user_id)?,
                    avatar_url: db.users.avatar_url(user_id)?,
                    is_direct: Some(is_direct),
                    third_party_invite: None,
                    blurhash: db.users.blurhash(user_id)?,
                    reason: None,
                })
                .expect("event is valid, we just created it"),
            )
            .expect("string is valid"),
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
