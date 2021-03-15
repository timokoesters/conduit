use super::State;
use crate::{
    client_server,
    pdu::{PduBuilder, PduEvent},
    utils, ConduitResult, Database, Error, Result, Ruma,
};
use log::{info, warn};
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
    events::{pdu::Pdu, room::member, EventType},
    serde::{to_canonical_value, CanonicalJsonObject, Raw},
    EventId, RoomId, RoomVersionId, ServerName, UserId,
};
use std::{
    collections::{BTreeMap, HashMap},
    convert::TryFrom,
    sync::Arc,
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/join", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn join_room_by_id_route(
    db: State<'_, Database>,
    body: Ruma<join_room_by_id::Request<'_>>,
) -> ConduitResult<join_room_by_id::Response> {
    join_room_by_id_helper(
        &db,
        body.sender_user.as_ref(),
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
#[tracing::instrument(skip(db, body))]
pub async fn join_room_by_id_or_alias_route(
    db: State<'_, Database>,
    body: Ruma<join_room_by_id_or_alias::Request<'_>>,
) -> ConduitResult<join_room_by_id_or_alias::Response> {
    let (servers, room_id) = match RoomId::try_from(body.room_id_or_alias.clone()) {
        Ok(room_id) => (vec![room_id.server_name().to_owned()], room_id),
        Err(room_alias) => {
            let response = client_server::get_alias_helper(&db, &room_alias).await?;

            (response.0.servers, response.0.room_id)
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

    db.flush().await?;

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
    db: State<'_, Database>,
    body: Ruma<leave_room::Request<'_>>,
) -> ConduitResult<leave_room::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut event = serde_json::from_value::<Raw<member::MemberEventContent>>(
        db.rooms
            .room_state_get(
                &body.room_id,
                &EventType::RoomMember,
                &sender_user.to_string(),
            )?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "Cannot leave a room you are not a member of.",
            ))?
            .1
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
            state_key: Some(sender_user.to_string()),
            redacts: None,
        },
        &sender_user,
        &body.room_id,
        &db,
    )?;

    db.flush().await?;

    Ok(leave_room::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/invite", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn invite_user_route(
    db: State<'_, Database>,
    body: Ruma<invite_user::Request<'_>>,
) -> ConduitResult<invite_user::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

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
            &sender_user,
            &body.room_id,
            &db,
        )?;

        db.flush().await?;

        Ok(invite_user::Response.into())
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
    db: State<'_, Database>,
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
            .1
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
        &sender_user,
        &body.room_id,
        &db,
    )?;

    db.flush().await?;

    Ok(kick_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/ban", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn ban_user_route(
    db: State<'_, Database>,
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
            }),
            |(_, event)| {
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
        &sender_user,
        &body.room_id,
        &db,
    )?;

    db.flush().await?;

    Ok(ban_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/unban", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn unban_user_route(
    db: State<'_, Database>,
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
            .1
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
        &sender_user,
        &body.room_id,
        &db,
    )?;

    db.flush().await?;

    Ok(unban_user::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/rooms/<_>/forget", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn forget_room_route(
    db: State<'_, Database>,
    body: Ruma<forget_room::Request<'_>>,
) -> ConduitResult<forget_room::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    db.rooms.forget(&body.room_id, &sender_user)?;

    db.flush().await?;

    Ok(forget_room::Response::new().into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/joined_rooms", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn joined_rooms_route(
    db: State<'_, Database>,
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
    db: State<'_, Database>,
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
    db: State<'_, Database>,
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
    servers: &[Box<ServerName>],
    _third_party_signed: Option<&IncomingThirdPartySigned>,
) -> ConduitResult<join_room_by_id::Response> {
    let sender_user = sender_user.expect("user is authenticated");

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

        let mut join_event_stub =
            serde_json::from_str::<CanonicalJsonObject>(make_join_response.event.json().get())
                .map_err(|_| {
                    Error::BadServerResponse("Invalid make_join event json received from server.")
                })?;

        join_event_stub.insert(
            "origin".to_owned(),
            to_canonical_value(db.globals.server_name())
                .map_err(|_| Error::bad_database("Invalid server name found"))?,
        );
        join_event_stub.insert(
            "origin_server_ts".to_owned(),
            to_canonical_value(utils::millis_since_unix_epoch())
                .expect("Timestamp is valid js_int value"),
        );
        join_event_stub.insert(
            "content".to_owned(),
            to_canonical_value(member::MemberEventContent {
                membership: member::MembershipState::Join,
                displayname: db.users.displayname(&sender_user)?,
                avatar_url: db.users.avatar_url(&sender_user)?,
                is_direct: None,
                third_party_invite: None,
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
            &RoomVersionId::Version6,
        )
        .expect("event is valid, we just created it");

        // Generate event id
        let event_id = EventId::try_from(&*format!(
            "${}",
            ruma::signatures::reference_hash(&join_event_stub, &RoomVersionId::Version6)
                .expect("ruma can calculate reference hashes")
        ))
        .expect("ruma's reference hashes are valid event ids");

        // Add event_id back
        join_event_stub.insert(
            "event_id".to_owned(),
            to_canonical_value(&event_id).expect("EventId is a valid CanonicalJsonValue"),
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

        let add_event_id = |pdu: &Raw<Pdu>| -> Result<(EventId, CanonicalJsonObject)> {
            let mut value = serde_json::from_str(pdu.json().get())
                .expect("converting raw jsons to values always works");
            let event_id = EventId::try_from(&*format!(
                "${}",
                ruma::signatures::reference_hash(&value, &RoomVersionId::Version6)
                    .expect("ruma can calculate reference hashes")
            ))
            .expect("ruma's reference hashes are valid event ids");

            value.insert(
                "event_id".to_owned(),
                to_canonical_value(&event_id)
                    .expect("a valid EventId can be converted to CanonicalJsonValue"),
            );

            Ok((event_id, value))
        };

        let count = db.globals.next_count()?;

        let mut pdu_id = room_id.as_bytes().to_vec();
        pdu_id.push(0xff);
        pdu_id.extend_from_slice(&count.to_be_bytes());

        let pdu = PduEvent::from_id_val(&event_id, join_event.clone())
            .map_err(|_| Error::BadServerResponse("Invalid PDU in send_join response."))?;

        let mut state = HashMap::new();

        for pdu in send_join_response
            .room_state
            .state
            .iter()
            .map(add_event_id)
            .map(|r| {
                let (event_id, value) = r?;
                PduEvent::from_id_val(&event_id, value.clone())
                    .map(|ev| (event_id, Arc::new(ev)))
                    .map_err(|e| {
                        warn!("{:?}: {}", value, e);
                        Error::BadServerResponse("Invalid PDU in send_join response.")
                    })
            })
        {
            let (id, pdu) = pdu?;
            info!("adding {} to outliers: {:#?}", id, pdu);
            db.rooms.add_pdu_outlier(&pdu)?;
            if let Some(state_key) = &pdu.state_key {
                if pdu.kind == EventType::RoomMember {
                    let target_user_id = UserId::try_from(state_key.clone()).map_err(|_| {
                        Error::BadServerResponse("Invalid user id in send_join response.")
                    })?;

                    // Update our membership info, we do this here incase a user is invited
                    // and immediately leaves we need the DB to record the invite event for auth
                    db.rooms.update_membership(
                        &pdu.room_id,
                        &target_user_id,
                        serde_json::from_value::<member::MemberEventContent>(pdu.content.clone())
                            .map_err(|_| {
                            Error::BadRequest(
                                ErrorKind::InvalidParam,
                                "Invalid member event content.",
                            )
                        })?,
                        &pdu.sender,
                        &db.account_data,
                        &db.globals,
                    )?;
                }
                let mut long_id = room_id.as_bytes().to_vec();
                long_id.push(0xff);
                long_id.extend_from_slice(id.as_bytes());
                state.insert((pdu.kind.clone(), state_key.clone()), long_id);
            }
        }

        state.insert(
            (
                pdu.kind.clone(),
                pdu.state_key.clone().expect("join event has state key"),
            ),
            pdu_id.clone(),
        );

        db.rooms.force_state(room_id, state, &db.globals)?;

        for pdu in send_join_response
            .room_state
            .auth_chain
            .iter()
            .map(add_event_id)
            .map(|r| {
                let (event_id, value) = r?;
                PduEvent::from_id_val(&event_id, value.clone())
                    .map(|ev| (event_id, Arc::new(ev)))
                    .map_err(|e| {
                        warn!("{:?}: {}", value, e);
                        Error::BadServerResponse("Invalid PDU in send_join response.")
                    })
            })
        {
            let (id, pdu) = pdu?;
            info!("adding {} to outliers: {:#?}", id, pdu);
            db.rooms.add_pdu_outlier(&pdu)?;
        }

        db.rooms.append_pdu(
            &pdu,
            utils::to_canonical_object(&pdu).expect("Pdu is valid canonical object"),
            db.globals.next_count()?,
            pdu_id.into(),
            &[pdu.event_id.clone()],
            db,
        )?;
    } else {
        let event = member::MemberEventContent {
            membership: member::MembershipState::Join,
            displayname: db.users.displayname(&sender_user)?,
            avatar_url: db.users.avatar_url(&sender_user)?,
            is_direct: None,
            third_party_invite: None,
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
        )?;
    }

    Ok(join_room_by_id::Response::new(room_id.clone()).into())
}
