use super::State;
use crate::{
    client_server,
    pdu::{PduBuilder, PduEvent},
    server_server, utils, ConduitResult, Database, Error, Result, Ruma,
};
use log::warn;
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
        federation,
    },
    events::pdu::Pdu,
    events::{room::member, EventType},
    EventId, Raw, RoomId, RoomVersionId, ServerName, UserId,
};
use state_res::StateEvent;
use std::{
    collections::BTreeMap, collections::HashMap, collections::HashSet, convert::TryFrom, iter,
    sync::Arc,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/join", data = "<body>")
)]
pub async fn join_room_by_id_route(
    db: State<'_, Database<'_>>,
    body: Ruma<join_room_by_id::Request<'_>>,
) -> ConduitResult<join_room_by_id::Response> {
    join_room_by_id_helper(
        &db,
        body.sender_id.as_ref(),
        &body.room_id,
        &[body.room_id.server_name().to_owned()],
        body.third_party_signed.as_ref(),
    )
    .await
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/join/<_>", data = "<body>")
)]
pub async fn join_room_by_id_or_alias_route(
    db: State<'_, Database<'_>>,
    body: Ruma<join_room_by_id_or_alias::Request<'_>>,
) -> ConduitResult<join_room_by_id_or_alias::Response> {
    let (servers, room_id) = match RoomId::try_from(body.room_id_or_alias.clone()) {
        Ok(room_id) => (vec![room_id.server_name().to_owned()], room_id),
        Err(room_alias) => {
            let response = client_server::get_alias_helper(&db, &room_alias).await?;

            (response.0.servers, response.0.room_id)
        }
    };

    Ok(join_room_by_id_or_alias::Response {
        room_id: join_room_by_id_helper(
            &db,
            body.sender_id.as_ref(),
            &room_id,
            &servers,
            body.third_party_signed.as_ref(),
        )
        .await?
        .0
        .room_id,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/leave", data = "<body>")
)]
pub async fn leave_room_route(
    db: State<'_, Database<'_>>,
    body: Ruma<leave_room::Request<'_>>,
) -> ConduitResult<leave_room::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    let mut event = serde_json::from_value::<Raw<member::MemberEventContent>>(
        db.rooms
            .room_state_get(
                &body.room_id,
                &EventType::RoomMember,
                &sender_id.to_string(),
            )?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "Cannot leave a room you are not a member of.",
            ))?
            .content,
    )
    .expect("from_value::<Raw<..>> can never fail")
    .deserialize()
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = member::MembershipState::Leave;

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(sender_id.to_string()),
            redacts: None,
        },
        &sender_id,
        &body.room_id,
        &db.globals,
        &db.sending,
        &db.account_data,
    )?;

    Ok(leave_room::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/invite", data = "<body>")
)]
pub async fn invite_user_route(
    db: State<'_, Database<'_>>,
    body: Ruma<invite_user::Request<'_>>,
) -> ConduitResult<invite_user::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    if let invite_user::IncomingInvitationRecipient::UserId { user_id } = &body.recipient {
        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(member::MemberEventContent {
                    membership: member::MembershipState::Invite,
                    displayname: db.users.displayname(&user_id)?,
                    avatar_url: db.users.avatar_url(&user_id)?,
                    is_direct: None,
                    third_party_invite: None,
                })
                .expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(user_id.to_string()),
                redacts: None,
            },
            &sender_id,
            &body.room_id,
            &db.globals,
            &db.sending,
            &db.account_data,
        )?;

        Ok(invite_user::Response.into())
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "User not found."))
    }
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/kick", data = "<body>")
)]
pub async fn kick_user_route(
    db: State<'_, Database<'_>>,
    body: Ruma<kick_user::Request<'_>>,
) -> ConduitResult<kick_user::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

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
            .content,
    )
    .expect("Raw::from_value always works")
    .deserialize()
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = ruma::events::room::member::MembershipState::Leave;
    // TODO: reason

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &sender_id,
        &body.room_id,
        &db.globals,
        &db.sending,
        &db.account_data,
    )?;

    Ok(kick_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/ban", data = "<body>")
)]
pub async fn ban_user_route(
    db: State<'_, Database<'_>>,
    body: Ruma<ban_user::Request<'_>>,
) -> ConduitResult<ban_user::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

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
            }),
            |event| {
                let mut event =
                    serde_json::from_value::<Raw<member::MemberEventContent>>(event.content)
                        .expect("Raw::from_value always works")
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid member event in database."))?;
                event.membership = ruma::events::room::member::MembershipState::Ban;
                Ok(event)
            },
        )?;

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &sender_id,
        &body.room_id,
        &db.globals,
        &db.sending,
        &db.account_data,
    )?;

    Ok(ban_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/unban", data = "<body>")
)]
pub async fn unban_user_route(
    db: State<'_, Database<'_>>,
    body: Ruma<unban_user::Request<'_>>,
) -> ConduitResult<unban_user::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

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
            .content,
    )
    .expect("from_value::<Raw<..>> can never fail")
    .deserialize()
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = ruma::events::room::member::MembershipState::Leave;

    db.rooms.build_and_append_pdu(
        PduBuilder {
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &sender_id,
        &body.room_id,
        &db.globals,
        &db.sending,
        &db.account_data,
    )?;

    Ok(unban_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/forget", data = "<body>")
)]
pub fn forget_room_route(
    db: State<'_, Database<'_>>,
    body: Ruma<forget_room::Request<'_>>,
) -> ConduitResult<forget_room::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    db.rooms.forget(&body.room_id, &sender_id)?;

    Ok(forget_room::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/joined_rooms", data = "<body>")
)]
pub fn joined_rooms_route(
    db: State<'_, Database<'_>>,
    body: Ruma<joined_rooms::Request>,
) -> ConduitResult<joined_rooms::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    Ok(joined_rooms::Response {
        joined_rooms: db
            .rooms
            .rooms_joined(&sender_id)
            .filter_map(|r| r.ok())
            .collect(),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/members", data = "<body>")
)]
pub fn get_member_events_route(
    db: State<'_, Database<'_>>,
    body: Ruma<get_member_events::Request<'_>>,
) -> ConduitResult<get_member_events::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(sender_id, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    Ok(get_member_events::Response {
        chunk: db
            .rooms
            .room_state_type(&body.room_id, &EventType::RoomMember)?
            .values()
            .map(|pdu| pdu.to_member_event())
            .collect(),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/rooms/<_>/joined_members", data = "<body>")
)]
pub fn joined_members_route(
    db: State<'_, Database<'_>>,
    body: Ruma<joined_members::Request<'_>>,
) -> ConduitResult<joined_members::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    if !db
        .rooms
        .is_joined(&sender_id, &body.room_id)
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

async fn join_room_by_id_helper(
    db: &Database<'static>,
    sender_id: Option<&UserId>,
    room_id: &RoomId,
    servers: &[Box<ServerName>],
    _third_party_signed: Option<&IncomingThirdPartySigned>,
) -> ConduitResult<join_room_by_id::Response> {
    let sender_id = sender_id.expect("user is authenticated");

    // Ask a remote server if we don't have this room
    if !db.rooms.exists(&room_id)? && room_id.server_name() != db.globals.server_name() {
        let mut make_join_response_and_server = Err(Error::BadServerResponse(
            "No server available to assist in joining.",
        ));

        for remote_server in servers {
            let make_join_response = server_server::send_request(
                &db.globals,
                remote_server.clone(),
                federation::membership::create_join_event_template::v1::Request {
                    room_id,
                    user_id: sender_id,
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

        let mut join_event_stub_value =
            serde_json::from_str::<serde_json::Value>(make_join_response.event.json().get())
                .map_err(|_| {
                    Error::BadServerResponse("Invalid make_join event json received from server.")
                })?;

        let join_event_stub =
            join_event_stub_value
                .as_object_mut()
                .ok_or(Error::BadServerResponse(
                    "Invalid make join event object received from server.",
                ))?;

        join_event_stub.insert(
            "origin".to_owned(),
            db.globals.server_name().to_owned().to_string().into(),
        );
        join_event_stub.insert(
            "origin_server_ts".to_owned(),
            utils::millis_since_unix_epoch().into(),
        );
        join_event_stub.insert(
            "content".to_owned(),
            serde_json::to_value(member::MemberEventContent {
                membership: member::MembershipState::Join,
                displayname: db.users.displayname(&sender_id)?,
                avatar_url: db.users.avatar_url(&sender_id)?,
                is_direct: None,
                third_party_invite: None,
            })
            .expect("event is valid, we just created it"),
        );

        // Generate event id
        let event_id = EventId::try_from(&*format!(
            "${}",
            ruma::signatures::reference_hash(&join_event_stub_value)
                .expect("ruma can calculate reference hashes")
        ))
        .expect("ruma's reference hashes are valid event ids");

        // We don't leave the event id into the pdu because that's only allowed in v1 or v2 rooms
        let join_event_stub = join_event_stub_value.as_object_mut().unwrap();
        join_event_stub.remove("event_id");

        ruma::signatures::hash_and_sign_event(
            db.globals.server_name().as_str(),
            db.globals.keypair(),
            &mut join_event_stub_value,
        )
        .expect("event is valid, we just created it");

        // Add event_id back
        let join_event_stub = join_event_stub_value.as_object_mut().unwrap();
        join_event_stub.insert("event_id".to_owned(), event_id.to_string().into());

        // It has enough fields to be called a proper event now
        let join_event = join_event_stub_value;

        let send_join_response = server_server::send_request(
            &db.globals,
            remote_server.clone(),
            federation::membership::create_join_event::v2::Request {
                room_id,
                event_id: &event_id,
                pdu_stub: PduEvent::convert_to_outgoing_federation_event(join_event.clone()),
            },
        )
        .await?;

        let add_event_id = |pdu: &Raw<Pdu>| {
            let mut value = serde_json::from_str(pdu.json().get())
                .expect("converting raw jsons to values always works");
            let event_id = EventId::try_from(&*format!(
                "${}",
                ruma::signatures::reference_hash(&value)
                    .expect("ruma can calculate reference hashes")
            ))
            .expect("ruma's reference hashes are valid event ids");

            value
                .as_object_mut()
                .ok_or_else(|| Error::BadServerResponse("PDU is not an object."))?
                .insert("event_id".to_owned(), event_id.to_string().into());

            Ok((event_id, value))
        };

        let room_state = send_join_response.room_state.state.iter().map(add_event_id);

        let state_events = room_state
            .clone()
            .map(|pdu: Result<(EventId, serde_json::Value)>| Ok(pdu?.0))
            .chain(iter::once(Ok(event_id.clone()))) // Add join event we just created
            .collect::<Result<HashSet<EventId>>>()?;

        let auth_chain = send_join_response
            .room_state
            .auth_chain
            .iter()
            .map(add_event_id);

        let mut event_map = room_state
            .chain(auth_chain)
            .chain(iter::once(Ok((event_id, join_event)))) // Add join event we just created
            .map(|r| {
                let (event_id, value) = r?;
                serde_json::from_value::<StateEvent>(value)
                    .map(|ev| (event_id, Arc::new(ev)))
                    .map_err(|e| {
                        warn!("{}", e);
                        Error::BadServerResponse("Invalid PDU bytes in send_join response.")
                    })
            })
            .collect::<Result<BTreeMap<EventId, Arc<StateEvent>>>>()?;

        let control_events = event_map
            .values()
            .filter(|pdu| pdu.is_power_event())
            .map(|pdu| pdu.event_id().clone())
            .collect::<Vec<_>>();

        // These events are not guaranteed to be sorted but they are resolved according to spec
        // we auth them anyways to weed out faulty/malicious server. The following is basically the
        // full state resolution algorithm.
        let event_ids = event_map.keys().cloned().collect::<Vec<_>>();

        let sorted_control_events = state_res::StateResolution::reverse_topological_power_sort(
            &room_id,
            &control_events,
            &mut event_map,
            &db.rooms,
            &event_ids,
        );

        // Auth check each event against the "partial" state created by the preceding events
        let resolved_control_events = state_res::StateResolution::iterative_auth_check(
            room_id,
            &RoomVersionId::Version6,
            &sorted_control_events,
            &BTreeMap::new(), // We have no "clean/resolved" events to add (these extend the `resolved_control_events`)
            &mut event_map,
            &db.rooms,
        )
        .expect("iterative auth check failed on resolved events");

        // This removes the control events that failed auth, leaving the resolved
        // to be mainline sorted
        let events_to_sort = event_map
            .keys()
            .filter(|id| {
                !sorted_control_events.contains(id)
                    || resolved_control_events.values().any(|rid| *id == rid)
            })
            .cloned()
            .collect::<Vec<_>>();

        let power_level = resolved_control_events.get(&(EventType::RoomPowerLevels, "".into()));
        // Sort the remaining non control events
        let sorted_event_ids = state_res::StateResolution::mainline_sort(
            room_id,
            &events_to_sort,
            power_level,
            &mut event_map,
            &db.rooms,
        );

        let resolved_events = state_res::StateResolution::iterative_auth_check(
            room_id,
            &RoomVersionId::Version6,
            &sorted_event_ids,
            &resolved_control_events,
            &mut event_map,
            &db.rooms,
        )
        .expect("iterative auth check failed on resolved events");

        let mut state = HashMap::new();

        // filter the events that failed the auth check keeping the remaining events
        // sorted correctly
        for ev_id in sorted_event_ids
            .iter()
            .filter(|id| resolved_events.values().any(|rid| rid == *id))
        {
            // this is a `state_res::StateEvent` that holds a `ruma::Pdu`
            let pdu = event_map
                .get(ev_id)
                .expect("Found event_id in sorted events that is not in resolved state");

            // We do not rebuild the PDU in this case only insert to DB
            let count = db.globals.next_count()?;
            let mut pdu_id = room_id.as_bytes().to_vec();
            pdu_id.push(0xff);
            pdu_id.extend_from_slice(&count.to_be_bytes());
            db.rooms.append_pdu(
                &PduEvent::from(&**pdu),
                &serde_json::to_value(&**pdu).expect("PDU is valid value"),
                count,
                pdu_id.clone().into(),
                &db.globals,
                &db.account_data,
                &db.sending,
            )?;

            if state_events.contains(ev_id) {
                state.insert((pdu.kind(), pdu.state_key()), pdu_id);
            }
        }

        db.rooms.force_state(room_id, state)?;
    } else {
        let event = member::MemberEventContent {
            membership: member::MembershipState::Join,
            displayname: db.users.displayname(&sender_id)?,
            avatar_url: db.users.avatar_url(&sender_id)?,
            is_direct: None,
            third_party_invite: None,
        };

        db.rooms.build_and_append_pdu(
            PduBuilder {
                event_type: EventType::RoomMember,
                content: serde_json::to_value(event).expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(sender_id.to_string()),
                redacts: None,
            },
            &sender_id,
            &room_id,
            &db.globals,
            &db.sending,
            &db.account_data,
        )?;
    }

    Ok(join_room_by_id::Response::new(room_id.clone()).into())
}
