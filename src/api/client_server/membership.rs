use ruma::{
    api::{
        client::{
            error::ErrorKind,
            membership::{
                ban_user, forget_room, get_member_events, invite_user, join_room_by_id,
                join_room_by_id_or_alias, joined_members, joined_rooms, kick_user, leave_room,
                unban_user, ThirdPartySigned,
            },
        },
        federation::{self, membership::create_invite},
    },
    canonical_json::to_canonical_value,
    events::{
        room::{
            join_rules::{AllowRule, JoinRule, RoomJoinRulesEventContent},
            member::{MembershipState, RoomMemberEventContent},
        },
        StateEventType, TimelineEventType,
    },
    serde::Base64,
    state_res, CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, OwnedRoomId,
    OwnedServerName, OwnedUserId, RoomId, RoomVersionId, UserId,
};
use serde_json::value::{to_raw_value, RawValue as RawJsonValue};
use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::{
    service::pdu::{gen_event_id_canonical_json, PduBuilder},
    services, utils, Error, PduEvent, Result, Ruma,
};

use super::get_alias_helper;

/// # `POST /_matrix/client/r0/rooms/{roomId}/join`
///
/// Tries to join the sender user into a room.
///
/// - If the server knowns about this room: creates the join event and does auth rules locally
/// - If the server does not know about the room: asks other servers over federation
pub async fn join_room_by_id_route(
    body: Ruma<join_room_by_id::v3::Request>,
) -> Result<join_room_by_id::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut servers = Vec::new(); // There is no body.server_name for /roomId/join
    servers.extend(
        services()
            .rooms
            .state_cache
            .invite_state(sender_user, &body.room_id)?
            .unwrap_or_default()
            .iter()
            .filter_map(|event| serde_json::from_str(event.json().get()).ok())
            .filter_map(|event: serde_json::Value| event.get("sender").cloned())
            .filter_map(|sender| sender.as_str().map(|s| s.to_owned()))
            .filter_map(|sender| UserId::parse(sender).ok())
            .map(|user| user.server_name().to_owned()),
    );

    servers.push(
        body.room_id
            .server_name()
            .expect("Room IDs should always have a server name")
            .into(),
    );

    join_room_by_id_helper(
        body.sender_user.as_deref(),
        &body.room_id,
        body.reason.clone(),
        &servers,
        body.third_party_signed.as_ref(),
    )
    .await
}

/// # `POST /_matrix/client/r0/join/{roomIdOrAlias}`
///
/// Tries to join the sender user into a room.
///
/// - If the server knowns about this room: creates the join event and does auth rules locally
/// - If the server does not know about the room: asks other servers over federation
pub async fn join_room_by_id_or_alias_route(
    body: Ruma<join_room_by_id_or_alias::v3::Request>,
) -> Result<join_room_by_id_or_alias::v3::Response> {
    let sender_user = body.sender_user.as_deref().expect("user is authenticated");
    let body = body.body;

    let (servers, room_id) = match OwnedRoomId::try_from(body.room_id_or_alias) {
        Ok(room_id) => {
            let mut servers = body.server_name.clone();
            servers.extend(
                services()
                    .rooms
                    .state_cache
                    .invite_state(sender_user, &room_id)?
                    .unwrap_or_default()
                    .iter()
                    .filter_map(|event| serde_json::from_str(event.json().get()).ok())
                    .filter_map(|event: serde_json::Value| event.get("sender").cloned())
                    .filter_map(|sender| sender.as_str().map(|s| s.to_owned()))
                    .filter_map(|sender| UserId::parse(sender).ok())
                    .map(|user| user.server_name().to_owned()),
            );

            servers.push(
                room_id
                    .server_name()
                    .expect("Room IDs should always have a server name")
                    .into(),
            );

            (servers, room_id)
        }
        Err(room_alias) => {
            let response = get_alias_helper(room_alias).await?;

            (response.servers, response.room_id)
        }
    };

    let join_room_response = join_room_by_id_helper(
        Some(sender_user),
        &room_id,
        body.reason.clone(),
        &servers,
        body.third_party_signed.as_ref(),
    )
    .await?;

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
    body: Ruma<leave_room::v3::Request>,
) -> Result<leave_room::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    leave_room(sender_user, &body.room_id, body.reason.clone()).await?;

    Ok(leave_room::v3::Response::new())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/invite`
///
/// Tries to send an invite event into the room.
pub async fn invite_user_route(
    body: Ruma<invite_user::v3::Request>,
) -> Result<invite_user::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if let invite_user::v3::InvitationRecipient::UserId { user_id } = &body.recipient {
        invite_helper(
            sender_user,
            user_id,
            &body.room_id,
            body.reason.clone(),
            false,
        )
        .await?;
        Ok(invite_user::v3::Response {})
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "User not found."))
    }
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/kick`
///
/// Tries to send a kick event into the room.
pub async fn kick_user_route(
    body: Ruma<kick_user::v3::Request>,
) -> Result<kick_user::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event: RoomMemberEventContent = serde_json::from_str(
        services()
            .rooms
            .state_accessor
            .room_state_get(
                &body.room_id,
                &StateEventType::RoomMember,
                body.user_id.as_ref(),
            )?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "Cannot kick a user who is not in the room.",
            ))?
            .content
            .get(),
    )
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    // If they are already kicked and the reason is unchanged, there isn't any point in sending a new event.
    if event.membership == MembershipState::Leave && event.reason == body.reason {
        return Ok(kick_user::v3::Response {});
    }

    let event = RoomMemberEventContent {
        is_direct: None,
        membership: MembershipState::Leave,
        third_party_invite: None,
        reason: body.reason.clone(),
        join_authorized_via_users_server: None,
        ..event
    };

    let mutex_state = Arc::clone(
        services()
            .globals
            .roomid_mutex_state
            .write()
            .await
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomMember,
                content: to_raw_value(&event).expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(body.user_id.to_string()),
                redacts: None,
            },
            sender_user,
            &body.room_id,
            &state_lock,
        )
        .await?;

    drop(state_lock);

    Ok(kick_user::v3::Response::new())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/ban`
///
/// Tries to send a ban event into the room.
pub async fn ban_user_route(body: Ruma<ban_user::v3::Request>) -> Result<ban_user::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event = if let Some(event) = services()
        .rooms
        .state_accessor
        .room_state_get(
            &body.room_id,
            &StateEventType::RoomMember,
            body.user_id.as_ref(),
        )?
        // Even when the previous member content is invalid, we should let the ban go through anyways.
        .and_then(|event| serde_json::from_str::<RoomMemberEventContent>(event.content.get()).ok())
    {
        // If they are already banned and the reason is unchanged, there isn't any point in sending a new event.
        if event.membership == MembershipState::Ban && event.reason == body.reason {
            return Ok(ban_user::v3::Response {});
        }

        RoomMemberEventContent {
            membership: MembershipState::Ban,
            join_authorized_via_users_server: None,
            reason: body.reason.clone(),
            third_party_invite: None,
            is_direct: None,
            avatar_url: event.avatar_url,
            displayname: event.displayname,
            blurhash: event.blurhash,
        }
    } else {
        RoomMemberEventContent {
            reason: body.reason.clone(),
            ..RoomMemberEventContent::new(MembershipState::Ban)
        }
    };

    let mutex_state = Arc::clone(
        services()
            .globals
            .roomid_mutex_state
            .write()
            .await
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomMember,
                content: to_raw_value(&event).expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(body.user_id.to_string()),
                redacts: None,
            },
            sender_user,
            &body.room_id,
            &state_lock,
        )
        .await?;

    drop(state_lock);

    Ok(ban_user::v3::Response::new())
}

/// # `POST /_matrix/client/r0/rooms/{roomId}/unban`
///
/// Tries to send an unban event into the room.
pub async fn unban_user_route(
    body: Ruma<unban_user::v3::Request>,
) -> Result<unban_user::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let event: RoomMemberEventContent = serde_json::from_str(
        services()
            .rooms
            .state_accessor
            .room_state_get(
                &body.room_id,
                &StateEventType::RoomMember,
                body.user_id.as_ref(),
            )?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "Cannot unban a user who is not banned.",
            ))?
            .content
            .get(),
    )
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    // If they are already unbanned and the reason is unchanged, there isn't any point in sending a new event.
    if event.membership == MembershipState::Leave && event.reason == body.reason {
        return Ok(unban_user::v3::Response {});
    }

    let event = RoomMemberEventContent {
        is_direct: None,
        membership: MembershipState::Leave,
        third_party_invite: None,
        reason: body.reason.clone(),
        join_authorized_via_users_server: None,
        ..event
    };

    let mutex_state = Arc::clone(
        services()
            .globals
            .roomid_mutex_state
            .write()
            .await
            .entry(body.room_id.clone())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    services()
        .rooms
        .timeline
        .build_and_append_pdu(
            PduBuilder {
                event_type: TimelineEventType::RoomMember,
                content: to_raw_value(&event).expect("event is valid, we just created it"),
                unsigned: None,
                state_key: Some(body.user_id.to_string()),
                redacts: None,
            },
            sender_user,
            &body.room_id,
            &state_lock,
        )
        .await?;

    drop(state_lock);

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
    body: Ruma<forget_room::v3::Request>,
) -> Result<forget_room::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    services()
        .rooms
        .state_cache
        .forget(&body.room_id, sender_user)?;

    Ok(forget_room::v3::Response::new())
}

/// # `POST /_matrix/client/r0/joined_rooms`
///
/// Lists all rooms the user has joined.
pub async fn joined_rooms_route(
    body: Ruma<joined_rooms::v3::Request>,
) -> Result<joined_rooms::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    Ok(joined_rooms::v3::Response {
        joined_rooms: services()
            .rooms
            .state_cache
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
    body: Ruma<get_member_events::v3::Request>,
) -> Result<get_member_events::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !services()
        .rooms
        .state_accessor
        .user_can_see_state_events(sender_user, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    Ok(get_member_events::v3::Response {
        chunk: services()
            .rooms
            .state_accessor
            .room_state_full(&body.room_id)
            .await?
            .iter()
            .filter(|(key, _)| key.0 == StateEventType::RoomMember)
            .map(|(_, pdu)| pdu.to_member_event())
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
    body: Ruma<joined_members::v3::Request>,
) -> Result<joined_members::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    if !services()
        .rooms
        .state_accessor
        .user_can_see_state_events(sender_user, &body.room_id)?
    {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    let mut joined = BTreeMap::new();
    for user_id in services()
        .rooms
        .state_cache
        .room_members(&body.room_id)
        .filter_map(|r| r.ok())
    {
        let display_name = services().users.displayname(&user_id)?;
        let avatar_url = services().users.avatar_url(&user_id)?;

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

async fn join_room_by_id_helper(
    sender_user: Option<&UserId>,
    room_id: &RoomId,
    reason: Option<String>,
    servers: &[OwnedServerName],
    _third_party_signed: Option<&ThirdPartySigned>,
) -> Result<join_room_by_id::v3::Response> {
    let sender_user = sender_user.expect("user is authenticated");

    if let Ok(true) = services().rooms.state_cache.is_joined(sender_user, room_id) {
        return Ok(join_room_by_id::v3::Response {
            room_id: room_id.into(),
        });
    }

    let mutex_state = Arc::clone(
        services()
            .globals
            .roomid_mutex_state
            .write()
            .await
            .entry(room_id.to_owned())
            .or_default(),
    );
    let state_lock = mutex_state.lock().await;

    // Ask a remote server if we are not participating in this room
    if !services()
        .rooms
        .state_cache
        .server_in_room(services().globals.server_name(), room_id)?
    {
        info!("Joining {room_id} over federation.");

        let (make_join_response, remote_server) =
            make_join_request(sender_user, room_id, servers).await?;

        info!("make_join finished");

        let room_version_id = match make_join_response.room_version {
            Some(room_version)
                if services()
                    .globals
                    .supported_room_versions()
                    .contains(&room_version) =>
            {
                room_version
            }
            _ => return Err(Error::BadServerResponse("Room version is not supported")),
        };

        let mut join_event_stub: CanonicalJsonObject =
            serde_json::from_str(make_join_response.event.get()).map_err(|_| {
                Error::BadServerResponse("Invalid make_join event json received from server.")
            })?;

        let join_authorized_via_users_server = join_event_stub
            .get("content")
            .map(|s| {
                s.as_object()?
                    .get("join_authorised_via_users_server")?
                    .as_str()
            })
            .and_then(|s| OwnedUserId::try_from(s.unwrap_or_default()).ok());

        // TODO: Is origin needed?
        join_event_stub.insert(
            "origin".to_owned(),
            CanonicalJsonValue::String(services().globals.server_name().as_str().to_owned()),
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
                displayname: services().users.displayname(sender_user)?,
                avatar_url: services().users.avatar_url(sender_user)?,
                is_direct: None,
                third_party_invite: None,
                blurhash: services().users.blurhash(sender_user)?,
                reason,
                join_authorized_via_users_server,
            })
            .expect("event is valid, we just created it"),
        );

        // We don't leave the event id in the pdu because that's only allowed in v1 or v2 rooms
        join_event_stub.remove("event_id");

        // In order to create a compatible ref hash (EventID) the `hashes` field needs to be present
        ruma::signatures::hash_and_sign_event(
            services().globals.server_name().as_str(),
            services().globals.keypair(),
            &mut join_event_stub,
            &room_version_id,
        )
        .expect("event is valid, we just created it");

        // Generate event id
        let event_id = format!(
            "${}",
            ruma::signatures::reference_hash(&join_event_stub, &room_version_id)
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
        let mut join_event = join_event_stub;

        info!("Asking {remote_server} for send_join");
        let send_join_response = services()
            .sending
            .send_federation_request(
                &remote_server,
                federation::membership::create_join_event::v2::Request {
                    room_id: room_id.to_owned(),
                    event_id: event_id.to_owned(),
                    pdu: PduEvent::convert_to_outgoing_federation_event(join_event.clone()),
                    omit_members: false,
                },
            )
            .await?;

        info!("send_join finished");

        if let Some(signed_raw) = &send_join_response.room_state.event {
            info!("There is a signed event. This room is probably using restricted joins. Adding signature to our event");
            let (signed_event_id, signed_value) =
                match gen_event_id_canonical_json(signed_raw, &room_version_id) {
                    Ok(t) => t,
                    Err(_) => {
                        // Event could not be converted to canonical json
                        return Err(Error::BadRequest(
                            ErrorKind::InvalidParam,
                            "Could not convert event to canonical json.",
                        ));
                    }
                };

            if signed_event_id != event_id {
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Server sent event with wrong event id",
                ));
            }

            match signed_value["signatures"]
                .as_object()
                .ok_or(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Server sent invalid signatures type",
                ))
                .and_then(|e| {
                    e.get(remote_server.as_str()).ok_or(Error::BadRequest(
                        ErrorKind::InvalidParam,
                        "Server did not send its signature",
                    ))
                }) {
                Ok(signature) => {
                    join_event
                        .get_mut("signatures")
                        .expect("we created a valid pdu")
                        .as_object_mut()
                        .expect("we created a valid pdu")
                        .insert(remote_server.to_string(), signature.clone());
                }
                Err(e) => {
                    warn!(
                        "Server {remote_server} sent invalid signature in sendjoin signatures for event {signed_value:?}: {e:?}",
                    );
                }
            }
        }

        services().rooms.short.get_or_create_shortroomid(room_id)?;

        info!("Parsing join event");
        let parsed_join_pdu = PduEvent::from_id_val(event_id, join_event.clone())
            .map_err(|_| Error::BadServerResponse("Invalid join event PDU."))?;

        let mut state = HashMap::new();
        let pub_key_map = RwLock::new(BTreeMap::new());

        info!("Fetching join signing keys");
        services()
            .rooms
            .event_handler
            .fetch_join_signing_keys(&send_join_response, &room_version_id, &pub_key_map)
            .await?;

        info!("Going through send_join response room_state");
        for result in send_join_response
            .room_state
            .state
            .iter()
            .map(|pdu| validate_and_add_event_id(pdu, &room_version_id, &pub_key_map))
        {
            let (event_id, value) = match result.await {
                Ok(t) => t,
                Err(_) => continue,
            };

            let pdu = PduEvent::from_id_val(&event_id, value.clone()).map_err(|e| {
                warn!("Invalid PDU in send_join response: {} {:?}", e, value);
                Error::BadServerResponse("Invalid PDU in send_join response.")
            })?;

            services()
                .rooms
                .outlier
                .add_pdu_outlier(&event_id, &value)?;
            if let Some(state_key) = &pdu.state_key {
                let shortstatekey = services()
                    .rooms
                    .short
                    .get_or_create_shortstatekey(&pdu.kind.to_string().into(), state_key)?;
                state.insert(shortstatekey, pdu.event_id.clone());
            }
        }

        info!("Going through send_join response auth_chain");
        for result in send_join_response
            .room_state
            .auth_chain
            .iter()
            .map(|pdu| validate_and_add_event_id(pdu, &room_version_id, &pub_key_map))
        {
            let (event_id, value) = match result.await {
                Ok(t) => t,
                Err(_) => continue,
            };

            services()
                .rooms
                .outlier
                .add_pdu_outlier(&event_id, &value)?;
        }

        info!("Running send_join auth check");
        let authenticated = state_res::event_auth::auth_check(
            &state_res::RoomVersion::new(&room_version_id).expect("room version is supported"),
            &parsed_join_pdu,
            None::<PduEvent>, // TODO: third party invite
            |k, s| {
                services()
                    .rooms
                    .timeline
                    .get_pdu(
                        state.get(
                            &services()
                                .rooms
                                .short
                                .get_or_create_shortstatekey(&k.to_string().into(), s)
                                .ok()?,
                        )?,
                    )
                    .ok()?
            },
        )
        .map_err(|e| {
            warn!("Auth check failed: {e}");
            Error::BadRequest(ErrorKind::InvalidParam, "Auth check failed")
        })?;

        if !authenticated {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Auth check failed",
            ));
        }

        info!("Saving state from send_join");
        let (statehash_before_join, new, removed) = services().rooms.state_compressor.save_state(
            room_id,
            Arc::new(
                state
                    .into_iter()
                    .map(|(k, id)| {
                        services()
                            .rooms
                            .state_compressor
                            .compress_state_event(k, &id)
                    })
                    .collect::<Result<_>>()?,
            ),
        )?;

        services()
            .rooms
            .state
            .force_state(room_id, statehash_before_join, new, removed, &state_lock)
            .await?;

        info!("Updating joined counts for new room");
        services().rooms.state_cache.update_joined_count(room_id)?;

        // We append to state before appending the pdu, so we don't have a moment in time with the
        // pdu without it's state. This is okay because append_pdu can't fail.
        let statehash_after_join = services().rooms.state.append_to_state(&parsed_join_pdu)?;

        info!("Appending new room join event");
        services()
            .rooms
            .timeline
            .append_pdu(
                &parsed_join_pdu,
                join_event,
                vec![(*parsed_join_pdu.event_id).to_owned()],
                &state_lock,
            )
            .await?;

        info!("Setting final room state for new room");
        // We set the room state after inserting the pdu, so that we never have a moment in time
        // where events in the current room state do not exist
        services()
            .rooms
            .state
            .set_room_state(room_id, statehash_after_join, &state_lock)?;
    } else {
        info!("We can join locally");

        let join_rules_event = services().rooms.state_accessor.room_state_get(
            room_id,
            &StateEventType::RoomJoinRules,
            "",
        )?;

        let join_rules_event_content: Option<RoomJoinRulesEventContent> = join_rules_event
            .as_ref()
            .map(|join_rules_event| {
                serde_json::from_str(join_rules_event.content.get()).map_err(|e| {
                    warn!("Invalid join rules event: {}", e);
                    Error::bad_database("Invalid join rules event in db.")
                })
            })
            .transpose()?;

        let restriction_rooms = match join_rules_event_content {
            Some(RoomJoinRulesEventContent {
                join_rule: JoinRule::Restricted(restricted),
            })
            | Some(RoomJoinRulesEventContent {
                join_rule: JoinRule::KnockRestricted(restricted),
            }) => restricted
                .allow
                .into_iter()
                .filter_map(|a| match a {
                    AllowRule::RoomMembership(r) => Some(r.room_id),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };

        let authorized_user = if restriction_rooms.iter().any(|restriction_room_id| {
            services()
                .rooms
                .state_cache
                .is_joined(sender_user, restriction_room_id)
                .unwrap_or(false)
        }) {
            let mut auth_user = None;
            for user in services()
                .rooms
                .state_cache
                .room_members(room_id)
                .filter_map(Result::ok)
                .collect::<Vec<_>>()
            {
                if user.server_name() == services().globals.server_name()
                    && services()
                        .rooms
                        .state_accessor
                        .user_can_invite(room_id, &user, sender_user, &state_lock)
                        .await
                        .unwrap_or(false)
                {
                    auth_user = Some(user);
                    break;
                }
            }
            auth_user
        } else {
            None
        };

        let event = RoomMemberEventContent {
            membership: MembershipState::Join,
            displayname: services().users.displayname(sender_user)?,
            avatar_url: services().users.avatar_url(sender_user)?,
            is_direct: None,
            third_party_invite: None,
            blurhash: services().users.blurhash(sender_user)?,
            reason: reason.clone(),
            join_authorized_via_users_server: authorized_user,
        };

        // Try normal join first
        let error = match services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomMember,
                    content: to_raw_value(&event).expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some(sender_user.to_string()),
                    redacts: None,
                },
                sender_user,
                room_id,
                &state_lock,
            )
            .await
        {
            Ok(_event_id) => return Ok(join_room_by_id::v3::Response::new(room_id.to_owned())),
            Err(e) => e,
        };

        if !restriction_rooms.is_empty()
            && servers
                .iter()
                .any(|s| *s != services().globals.server_name())
        {
            info!(
                "We couldn't do the join locally, maybe federation can help to satisfy the restricted join requirements"
            );
            let (make_join_response, remote_server) =
                make_join_request(sender_user, room_id, servers).await?;

            let room_version_id = match make_join_response.room_version {
                Some(room_version_id)
                    if services()
                        .globals
                        .supported_room_versions()
                        .contains(&room_version_id) =>
                {
                    room_version_id
                }
                _ => return Err(Error::BadServerResponse("Room version is not supported")),
            };
            let mut join_event_stub: CanonicalJsonObject =
                serde_json::from_str(make_join_response.event.get()).map_err(|_| {
                    Error::BadServerResponse("Invalid make_join event json received from server.")
                })?;
            let join_authorized_via_users_server = join_event_stub
                .get("content")
                .map(|s| {
                    s.as_object()?
                        .get("join_authorised_via_users_server")?
                        .as_str()
                })
                .and_then(|s| OwnedUserId::try_from(s.unwrap_or_default()).ok());
            // TODO: Is origin needed?
            join_event_stub.insert(
                "origin".to_owned(),
                CanonicalJsonValue::String(services().globals.server_name().as_str().to_owned()),
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
                    displayname: services().users.displayname(sender_user)?,
                    avatar_url: services().users.avatar_url(sender_user)?,
                    is_direct: None,
                    third_party_invite: None,
                    blurhash: services().users.blurhash(sender_user)?,
                    reason,
                    join_authorized_via_users_server,
                })
                .expect("event is valid, we just created it"),
            );

            // We don't leave the event id in the pdu because that's only allowed in v1 or v2 rooms
            join_event_stub.remove("event_id");

            // In order to create a compatible ref hash (EventID) the `hashes` field needs to be present
            ruma::signatures::hash_and_sign_event(
                services().globals.server_name().as_str(),
                services().globals.keypair(),
                &mut join_event_stub,
                &room_version_id,
            )
            .expect("event is valid, we just created it");

            // Generate event id
            let event_id = format!(
                "${}",
                ruma::signatures::reference_hash(&join_event_stub, &room_version_id)
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

            let send_join_response = services()
                .sending
                .send_federation_request(
                    &remote_server,
                    federation::membership::create_join_event::v2::Request {
                        room_id: room_id.to_owned(),
                        event_id: event_id.to_owned(),
                        pdu: PduEvent::convert_to_outgoing_federation_event(join_event.clone()),
                        omit_members: false,
                    },
                )
                .await?;

            if let Some(signed_raw) = send_join_response.room_state.event {
                let (signed_event_id, signed_value) =
                    match gen_event_id_canonical_json(&signed_raw, &room_version_id) {
                        Ok(t) => t,
                        Err(_) => {
                            // Event could not be converted to canonical json
                            return Err(Error::BadRequest(
                                ErrorKind::InvalidParam,
                                "Could not convert event to canonical json.",
                            ));
                        }
                    };

                if signed_event_id != event_id {
                    return Err(Error::BadRequest(
                        ErrorKind::InvalidParam,
                        "Server sent event with wrong event id",
                    ));
                }

                drop(state_lock);
                let pub_key_map = RwLock::new(BTreeMap::new());
                services()
                    .rooms
                    .event_handler
                    .handle_incoming_pdu(
                        &remote_server,
                        &signed_event_id,
                        room_id,
                        signed_value,
                        true,
                        &pub_key_map,
                    )
                    .await?;
            } else {
                return Err(error);
            }
        } else {
            return Err(error);
        }
    }

    Ok(join_room_by_id::v3::Response::new(room_id.to_owned()))
}

async fn make_join_request(
    sender_user: &UserId,
    room_id: &RoomId,
    servers: &[OwnedServerName],
) -> Result<(
    federation::membership::prepare_join_event::v1::Response,
    OwnedServerName,
)> {
    let mut make_join_response_and_server = Err(Error::BadServerResponse(
        "No server available to assist in joining.",
    ));

    for remote_server in servers {
        if remote_server == services().globals.server_name() {
            continue;
        }
        info!("Asking {remote_server} for make_join");
        let make_join_response = services()
            .sending
            .send_federation_request(
                remote_server,
                federation::membership::prepare_join_event::v1::Request {
                    room_id: room_id.to_owned(),
                    user_id: sender_user.to_owned(),
                    ver: services().globals.supported_room_versions(),
                },
            )
            .await;

        make_join_response_and_server = make_join_response.map(|r| (r, remote_server.clone()));

        if make_join_response_and_server.is_ok() {
            break;
        }
    }

    make_join_response_and_server
}

async fn validate_and_add_event_id(
    pdu: &RawJsonValue,
    room_version: &RoomVersionId,
    pub_key_map: &RwLock<BTreeMap<String, BTreeMap<String, Base64>>>,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
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

    let back_off = |id| async {
        match services()
            .globals
            .bad_event_ratelimiter
            .write()
            .await
            .entry(id)
        {
            Entry::Vacant(e) => {
                e.insert((Instant::now(), 1));
            }
            Entry::Occupied(mut e) => *e.get_mut() = (Instant::now(), e.get().1 + 1),
        }
    };

    if let Some((time, tries)) = services()
        .globals
        .bad_event_ratelimiter
        .read()
        .await
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

    if let Err(e) = ruma::signatures::verify_event(&*pub_key_map.read().await, &value, room_version)
    {
        warn!("Event {} failed verification {:?} {}", event_id, pdu, e);
        back_off(event_id).await;
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
    reason: Option<String>,
    is_direct: bool,
) -> Result<()> {
    if user_id.server_name() != services().globals.server_name() {
        let (pdu, pdu_json, invite_room_state) = {
            let mutex_state = Arc::clone(
                services()
                    .globals
                    .roomid_mutex_state
                    .write()
                    .await
                    .entry(room_id.to_owned())
                    .or_default(),
            );
            let state_lock = mutex_state.lock().await;

            let content = to_raw_value(&RoomMemberEventContent {
                avatar_url: None,
                displayname: None,
                is_direct: Some(is_direct),
                membership: MembershipState::Invite,
                third_party_invite: None,
                blurhash: None,
                reason,
                join_authorized_via_users_server: None,
            })
            .expect("member event is valid value");

            let (pdu, pdu_json) = services().rooms.timeline.create_hash_and_sign_event(
                PduBuilder {
                    event_type: TimelineEventType::RoomMember,
                    content,
                    unsigned: None,
                    state_key: Some(user_id.to_string()),
                    redacts: None,
                },
                sender_user,
                room_id,
                &state_lock,
            )?;

            let invite_room_state = services().rooms.state.calculate_invite_state(&pdu)?;

            drop(state_lock);

            (pdu, pdu_json, invite_room_state)
        };

        let room_version_id = services().rooms.state.get_room_version(room_id)?;

        let response = services()
            .sending
            .send_federation_request(
                user_id.server_name(),
                create_invite::v2::Request {
                    room_id: room_id.to_owned(),
                    event_id: (*pdu.event_id).to_owned(),
                    room_version: room_version_id.clone(),
                    event: PduEvent::convert_to_outgoing_federation_event(pdu_json.clone()),
                    invite_room_state,
                },
            )
            .await?;

        let pub_key_map = RwLock::new(BTreeMap::new());

        // We do not add the event_id field to the pdu here because of signature and hashes checks
        let (event_id, value) = match gen_event_id_canonical_json(&response.event, &room_version_id)
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

        if *pdu.event_id != *event_id {
            warn!("Server {} changed invite event, that's not allowed in the spec: ours: {:?}, theirs: {:?}", user_id.server_name(), pdu_json, value);
        }

        let origin: OwnedServerName = serde_json::from_value(
            serde_json::to_value(value.get("origin").ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event needs an origin field.",
            ))?)
            .expect("CanonicalJson is valid json value"),
        )
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Origin field is invalid."))?;

        let pdu_id: Vec<u8> = services()
            .rooms
            .event_handler
            .handle_incoming_pdu(&origin, &event_id, room_id, value, true, &pub_key_map)
            .await?
            .ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Could not accept incoming PDU as timeline event.",
            ))?;

        // Bind to variable because of lifetimes
        let servers = services()
            .rooms
            .state_cache
            .room_servers(room_id)
            .filter_map(|r| r.ok())
            .filter(|server| &**server != services().globals.server_name());

        services().sending.send_pdu(servers, &pdu_id)?;
    } else {
        if !services()
            .rooms
            .state_cache
            .is_joined(sender_user, room_id)?
        {
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "You don't have permission to view this room.",
            ));
        }

        let mutex_state = Arc::clone(
            services()
                .globals
                .roomid_mutex_state
                .write()
                .await
                .entry(room_id.to_owned())
                .or_default(),
        );
        let state_lock = mutex_state.lock().await;

        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomMember,
                    content: to_raw_value(&RoomMemberEventContent {
                        membership: MembershipState::Invite,
                        displayname: services().users.displayname(user_id)?,
                        avatar_url: services().users.avatar_url(user_id)?,
                        is_direct: Some(is_direct),
                        third_party_invite: None,
                        blurhash: services().users.blurhash(user_id)?,
                        reason,
                        join_authorized_via_users_server: None,
                    })
                    .expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some(user_id.to_string()),
                    redacts: None,
                },
                sender_user,
                room_id,
                &state_lock,
            )
            .await?;

        // Critical point ends
        drop(state_lock);
    }

    Ok(())
}

// Make a user leave all their joined rooms
pub async fn leave_all_rooms(user_id: &UserId) -> Result<()> {
    let all_rooms = services()
        .rooms
        .state_cache
        .rooms_joined(user_id)
        .chain(
            services()
                .rooms
                .state_cache
                .rooms_invited(user_id)
                .map(|t| t.map(|(r, _)| r)),
        )
        .collect::<Vec<_>>();

    for room_id in all_rooms {
        let room_id = match room_id {
            Ok(room_id) => room_id,
            Err(_) => continue,
        };

        let _ = leave_room(user_id, &room_id, None).await;
    }

    Ok(())
}

pub async fn leave_room(user_id: &UserId, room_id: &RoomId, reason: Option<String>) -> Result<()> {
    // Ask a remote server if we don't have this room
    if !services()
        .rooms
        .state_cache
        .server_in_room(services().globals.server_name(), room_id)?
    {
        if let Err(e) = remote_leave_room(user_id, room_id).await {
            warn!("Failed to leave room {} remotely: {}", user_id, e);
            // Don't tell the client about this error
        }

        let last_state = services()
            .rooms
            .state_cache
            .invite_state(user_id, room_id)?
            .map_or_else(
                || services().rooms.state_cache.left_state(user_id, room_id),
                |s| Ok(Some(s)),
            )?;

        // We always drop the invite, we can't rely on other servers
        services().rooms.state_cache.update_membership(
            room_id,
            user_id,
            MembershipState::Leave,
            user_id,
            last_state,
            true,
        )?;
    } else {
        let mutex_state = Arc::clone(
            services()
                .globals
                .roomid_mutex_state
                .write()
                .await
                .entry(room_id.to_owned())
                .or_default(),
        );
        let state_lock = mutex_state.lock().await;

        let member_event = services().rooms.state_accessor.room_state_get(
            room_id,
            &StateEventType::RoomMember,
            user_id.as_str(),
        )?;

        // Fix for broken rooms
        let member_event = match member_event {
            None => {
                error!("Trying to leave a room you are not a member of.");

                services().rooms.state_cache.update_membership(
                    room_id,
                    user_id,
                    MembershipState::Leave,
                    user_id,
                    None,
                    true,
                )?;
                return Ok(());
            }
            Some(e) => e,
        };

        let event = RoomMemberEventContent {
            is_direct: None,
            membership: MembershipState::Leave,
            third_party_invite: None,
            reason,
            join_authorized_via_users_server: None,
            ..serde_json::from_str(member_event.content.get())
                .map_err(|_| Error::bad_database("Invalid member event in database."))?
        };

        services()
            .rooms
            .timeline
            .build_and_append_pdu(
                PduBuilder {
                    event_type: TimelineEventType::RoomMember,
                    content: to_raw_value(&event).expect("event is valid, we just created it"),
                    unsigned: None,
                    state_key: Some(user_id.to_string()),
                    redacts: None,
                },
                user_id,
                room_id,
                &state_lock,
            )
            .await?;
    }

    Ok(())
}

async fn remote_leave_room(user_id: &UserId, room_id: &RoomId) -> Result<()> {
    let mut make_leave_response_and_server = Err(Error::BadServerResponse(
        "No server available to assist in leaving.",
    ));

    let invite_state = services()
        .rooms
        .state_cache
        .invite_state(user_id, room_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::BadState,
            "User is not invited.",
        ))?;

    let servers: HashSet<_> = invite_state
        .iter()
        .filter_map(|event| serde_json::from_str(event.json().get()).ok())
        .filter_map(|event: serde_json::Value| event.get("sender").cloned())
        .filter_map(|sender| sender.as_str().map(|s| s.to_owned()))
        .filter_map(|sender| UserId::parse(sender).ok())
        .map(|user| user.server_name().to_owned())
        .collect();

    for remote_server in servers {
        let make_leave_response = services()
            .sending
            .send_federation_request(
                &remote_server,
                federation::membership::prepare_leave_event::v1::Request {
                    room_id: room_id.to_owned(),
                    user_id: user_id.to_owned(),
                },
            )
            .await;

        make_leave_response_and_server = make_leave_response.map(|r| (r, remote_server));

        if make_leave_response_and_server.is_ok() {
            break;
        }
    }

    let (make_leave_response, remote_server) = make_leave_response_and_server?;

    let room_version_id = match make_leave_response.room_version {
        Some(version)
            if services()
                .globals
                .supported_room_versions()
                .contains(&version) =>
        {
            version
        }
        _ => return Err(Error::BadServerResponse("Room version is not supported")),
    };

    let mut leave_event_stub = serde_json::from_str::<CanonicalJsonObject>(
        make_leave_response.event.get(),
    )
    .map_err(|_| Error::BadServerResponse("Invalid make_leave event json received from server."))?;

    // TODO: Is origin needed?
    leave_event_stub.insert(
        "origin".to_owned(),
        CanonicalJsonValue::String(services().globals.server_name().as_str().to_owned()),
    );
    leave_event_stub.insert(
        "origin_server_ts".to_owned(),
        CanonicalJsonValue::Integer(
            utils::millis_since_unix_epoch()
                .try_into()
                .expect("Timestamp is valid js_int value"),
        ),
    );
    // We don't leave the event id in the pdu because that's only allowed in v1 or v2 rooms
    leave_event_stub.remove("event_id");

    // In order to create a compatible ref hash (EventID) the `hashes` field needs to be present
    ruma::signatures::hash_and_sign_event(
        services().globals.server_name().as_str(),
        services().globals.keypair(),
        &mut leave_event_stub,
        &room_version_id,
    )
    .expect("event is valid, we just created it");

    // Generate event id
    let event_id = EventId::parse(format!(
        "${}",
        ruma::signatures::reference_hash(&leave_event_stub, &room_version_id)
            .expect("ruma can calculate reference hashes")
    ))
    .expect("ruma's reference hashes are valid event ids");

    // Add event_id back
    leave_event_stub.insert(
        "event_id".to_owned(),
        CanonicalJsonValue::String(event_id.as_str().to_owned()),
    );

    // It has enough fields to be called a proper event now
    let leave_event = leave_event_stub;

    services()
        .sending
        .send_federation_request(
            &remote_server,
            federation::membership::create_leave_event::v2::Request {
                room_id: room_id.to_owned(),
                event_id,
                pdu: PduEvent::convert_to_outgoing_federation_event(leave_event.clone()),
            },
        )
        .await?;

    Ok(())
}
