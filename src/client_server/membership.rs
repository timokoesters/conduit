use super::State;
use crate::{
    client_server,
    pdu::{PduBuilder, PduEvent},
    server_server, utils, ConduitResult, Database, Error, Ruma,
};
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
    events::{room::member, EventType},
    EventId, Raw, RoomId, RoomVersionId, UserId,
};
use state_res::StateEvent;

use std::{collections::BTreeMap, convert::TryFrom};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/join", data = "<body>")
)]
pub async fn join_room_by_id_route(
    db: State<'_, Database>,
    body: Ruma<join_room_by_id::IncomingRequest>,
) -> ConduitResult<join_room_by_id::Response> {
    join_room_by_id_helper(
        &db,
        body.sender_id.as_ref(),
        &body.room_id,
        body.third_party_signed.as_ref(),
    )
    .await
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/join/<_>", data = "<body>")
)]
pub async fn join_room_by_id_or_alias_route(
    db: State<'_, Database>,
    db2: State<'_, Database>,
    body: Ruma<join_room_by_id_or_alias::IncomingRequest>,
) -> ConduitResult<join_room_by_id_or_alias::Response> {
    let room_id = match RoomId::try_from(body.room_id_or_alias.clone()) {
        Ok(room_id) => room_id,
        Err(room_alias) => {
            client_server::get_alias_helper(db, &room_alias)
                .await?
                .0
                .room_id
        }
    };

    Ok(join_room_by_id_or_alias::Response {
        room_id: join_room_by_id_helper(
            &db2,
            body.sender_id.as_ref(),
            &room_id,
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
pub fn leave_room_route(
    db: State<'_, Database>,
    body: Ruma<leave_room::IncomingRequest>,
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
            room_id: body.room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(sender_id.to_string()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    Ok(leave_room::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/invite", data = "<body>")
)]
pub fn invite_user_route(
    db: State<'_, Database>,
    body: Ruma<invite_user::Request>,
) -> ConduitResult<invite_user::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    if let invite_user::InvitationRecipient::UserId { user_id } = &body.recipient {
        db.rooms.build_and_append_pdu(
            PduBuilder {
                room_id: body.room_id.clone(),
                sender: sender_id.clone(),
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
            &db.globals,
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
pub fn kick_user_route(
    db: State<'_, Database>,
    body: Ruma<kick_user::IncomingRequest>,
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
            room_id: body.room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    Ok(kick_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/ban", data = "<body>")
)]
pub fn ban_user_route(
    db: State<'_, Database>,
    body: Ruma<ban_user::IncomingRequest>,
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
            room_id: body.room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    Ok(ban_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/unban", data = "<body>")
)]
pub fn unban_user_route(
    db: State<'_, Database>,
    body: Ruma<unban_user::IncomingRequest>,
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
            room_id: body.room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(body.user_id.to_string()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    Ok(unban_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/forget", data = "<body>")
)]
pub fn forget_room_route(
    db: State<'_, Database>,
    body: Ruma<forget_room::IncomingRequest>,
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
    db: State<'_, Database>,
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
    db: State<'_, Database>,
    body: Ruma<get_member_events::Request>,
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
    db: State<'_, Database>,
    body: Ruma<joined_members::Request>,
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
    db: &Database,
    sender_id: Option<&UserId>,
    room_id: &RoomId,
    _third_party_signed: Option<&IncomingThirdPartySigned>,
) -> ConduitResult<join_room_by_id::Response> {
    let sender_id = sender_id.expect("user is authenticated");

    // Ask a remote server if we don't have this room
    if !db.rooms.exists(&room_id)? && room_id.server_name() != db.globals.server_name() {
        let make_join_response = server_server::send_request(
            &db,
            room_id.server_name().to_string(),
            federation::membership::create_join_event_template::v1::Request {
                room_id: room_id.clone(),
                user_id: sender_id.clone(),
                ver: vec![RoomVersionId::Version5, RoomVersionId::Version6],
            },
        )
        .await?;

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

        let send_join_response = server_server::send_request(
            &db,
            room_id.server_name().to_string(),
            federation::membership::create_join_event::v1::Request {
                room_id: room_id.clone(),
                event_id,
                pdu_stub: serde_json::from_value(join_event_stub_value)
                    .expect("we just created this event"),
            },
        )
        .await?;

        dbg!(&send_join_response);
        // todo!("Take send_join_response and 'create' the room using that data");

        let mut event_map = send_join_response
            .room_state
            .state
            .iter()
            .map(|pdu| {
                pdu.deserialize()
                    .map(StateEvent::Full)
                    .map(|ev| (ev.event_id(), ev))
            })
            .collect::<Result<BTreeMap<EventId, StateEvent>, _>>()
            .map_err(|_| Error::bad_database("Invalid PDU found in db."))?;

        let _auth_chain = send_join_response
            .room_state
            .auth_chain
            .iter()
            .flat_map(|pdu| pdu.deserialize().ok())
            .map(StateEvent::Full)
            .collect::<Vec<_>>();

        // TODO make StateResolution's methods free functions ? or no self param ?
        let sorted_events_ids = state_res::StateResolution::default()
            .reverse_topological_power_sort(
                &room_id,
                &event_map.keys().cloned().collect::<Vec<_>>(),
                &mut event_map,
                &db.rooms,
                &[], // TODO auth_diff: is this none since we have a set of resolved events we only want to sort
            );

        for ev_id in &sorted_events_ids {
            // this is a `state_res::StateEvent` that holds a `ruma::Pdu`
            let pdu = event_map
                .get(ev_id)
                .expect("Found event_id in sorted events that is not in resolved state");

            // We do not rebuild the PDU in this case only insert to DB
            db.rooms
                .append_pdu(PduEvent::try_from(pdu)?, &db.globals, &db.account_data)?;
        }
    }

    let event = member::MemberEventContent {
        membership: member::MembershipState::Join,
        displayname: db.users.displayname(&sender_id)?,
        avatar_url: db.users.avatar_url(&sender_id)?,
        is_direct: None,
        third_party_invite: None,
    };

    db.rooms.build_and_append_pdu(
        PduBuilder {
            room_id: room_id.clone(),
            sender: sender_id.clone(),
            event_type: EventType::RoomMember,
            content: serde_json::to_value(event).expect("event is valid, we just created it"),
            unsigned: None,
            state_key: Some(sender_id.to_string()),
            redacts: None,
        },
        &db.globals,
        &db.account_data,
    )?;

    Ok(join_room_by_id::Response::new(room_id.clone()).into())
}
