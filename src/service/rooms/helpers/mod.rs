use std::{
    collections::{hash_map::Entry, BTreeMap, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use ruma::{
    api::{
        client::{
            error::ErrorKind,
            membership::{join_room_by_id, ThirdPartySigned},
        },
        federation,
    },
    canonical_json::to_canonical_value,
    events::{
        room::{
            join_rules::{AllowRule, JoinRule, RoomJoinRulesEventContent},
            member::{MembershipState, RoomMemberEventContent},
        },
        TimelineEventType,
    },
    room_version_rules::RoomVersionRules,
    state_res, CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch,
    OwnedEventId, OwnedServerName, OwnedUserId, RoomId, RoomVersionId, UserId,
};
use serde_json::value::{to_raw_value, RawValue as RawJsonValue};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::{
    service::{
        globals::SigningKeys,
        pdu::{gen_event_id_canonical_json, PduBuilder},
    },
    services, utils, Error, PduEvent, Result,
};

pub struct Service;

impl Service {
    /// Attempts to join a room.
    /// If the room cannot be joined locally, it attempts to join over federation, solely using the
    /// specified servers
    #[tracing::instrument(skip(self, reason, servers, _third_party_signed))]
    pub async fn join_room_by_id(
        &self,
        sender_user: &UserId,
        room_id: &RoomId,
        reason: Option<String>,
        servers: &[OwnedServerName],
        _third_party_signed: Option<&ThirdPartySigned>,
    ) -> Result<join_room_by_id::v3::Response, Error> {
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
            let room_version_rules = room_version_id
                .rules()
                .expect("Supported room version has rules");

            let (event_id, mut join_event, _) = self.populate_membership_template(
                &make_join_response.event,
                sender_user,
                reason,
                &room_version_rules,
                MembershipState::Join,
            )?;

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
                    match gen_event_id_canonical_json(signed_raw, &room_version_rules) {
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
            let parsed_join_pdu = PduEvent::from_id_val(&event_id, join_event.clone())
                .map_err(|_| Error::BadServerResponse("Invalid join event PDU."))?;

            let mut state = HashMap::new();
            let pub_key_map = RwLock::new(BTreeMap::new());

            info!("Fetching join signing keys");
            services()
                .rooms
                .event_handler
                .fetch_join_signing_keys(
                    &send_join_response,
                    &room_version_id
                        .rules()
                        .expect("Supported room version has rules"),
                    &pub_key_map,
                )
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
            if let Err(e) = state_res::event_auth::auth_check(
                &room_version_id
                    .rules()
                    .expect("Supported room version has rules")
                    .authorization,
                &parsed_join_pdu,
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
            ) {
                warn!("Auth check failed: {e}");
                return Err(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Auth check failed",
                ));
            };

            info!("Saving state from send_join");
            let (statehash_before_join, new, removed) =
                services().rooms.state_compressor.save_state(
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

            let join_rules_event_content =
                services().rooms.state_accessor.get_join_rules(room_id)?;

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
            let Err(error) = services()
                .rooms
                .timeline
                .build_and_append_pdu(
                    PduBuilder {
                        event_type: TimelineEventType::RoomMember,
                        content: to_raw_value(&event).expect("event is valid, we just created it"),
                        unsigned: None,
                        state_key: Some(sender_user.to_string()),
                        redacts: None,
                        timestamp: None,
                    },
                    sender_user,
                    room_id,
                    &state_lock,
                )
                .await
            else {
                return Ok(join_room_by_id::v3::Response::new(room_id.to_owned()));
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

                let (event_id, join_event, restricted_join) = self.populate_membership_template(
                    &make_join_response.event,
                    sender_user,
                    reason,
                    &room_version_id
                        .rules()
                        .expect("Supported room version has rules"),
                    MembershipState::Join,
                )?;

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

                let pdu = if let Some(signed_raw) = send_join_response.room_state.event {
                    let (signed_event_id, signed_pdu) = gen_event_id_canonical_json(
                        &signed_raw,
                        &room_version_id
                            .rules()
                            .expect("Supported room version has rules"),
                    )?;

                    if signed_event_id != event_id {
                        return Err(Error::BadServerResponse(
                            "Server sent event with wrong event id",
                        ));
                    }

                    signed_pdu
                } else if restricted_join {
                    return Err(Error::BadServerResponse(
                        "No signed event was returned, despite just performing a restricted join",
                    ));
                } else {
                    join_event
                };

                drop(state_lock);
                let pub_key_map = RwLock::new(BTreeMap::new());
                services()
                    .rooms
                    .event_handler
                    .handle_incoming_pdu(
                        &remote_server,
                        &event_id,
                        room_id,
                        pdu,
                        true,
                        &pub_key_map,
                    )
                    .await?;
            } else {
                return Err(error);
            }
        }

        Ok(join_room_by_id::v3::Response::new(room_id.to_owned()))
    }

    /// Takes a membership template, as returned from the `/federation/*/make_*` endpoints, and
    /// populates them to the point as to where they are a full pdu, ready to be appended to the timeline
    ///
    /// Returns the event id, the pdu, and whether this event is a restricted join
    pub fn populate_membership_template(
        &self,
        member_template: &RawJsonValue,
        sender_user: &UserId,
        reason: Option<String>,
        room_version_rules: &RoomVersionRules,
        membership: MembershipState,
    ) -> Result<(OwnedEventId, BTreeMap<String, CanonicalJsonValue>, bool), Error> {
        let mut member_event_stub: CanonicalJsonObject =
            serde_json::from_str(member_template.get()).map_err(|_| {
                Error::BadServerResponse("Invalid make_knock event json received from server.")
            })?;

        let join_authorized_via_users_server = member_event_stub
            .get("content")
            .map(|s| {
                s.as_object()?
                    .get("join_authorised_via_users_server")?
                    .as_str()
            })
            .and_then(|s| OwnedUserId::try_from(s.unwrap_or_default()).ok());

        let restricted_join = join_authorized_via_users_server.is_some();

        member_event_stub.insert(
            "origin".to_owned(),
            CanonicalJsonValue::String(services().globals.server_name().as_str().to_owned()),
        );

        member_event_stub.insert(
            "origin_server_ts".to_owned(),
            CanonicalJsonValue::Integer(
                utils::millis_since_unix_epoch()
                    .try_into()
                    .expect("Timestamp is valid js_int value"),
            ),
        );

        member_event_stub.insert(
            "content".to_owned(),
            to_canonical_value(RoomMemberEventContent {
                membership,
                displayname: services().users.displayname(sender_user)?,
                avatar_url: services().users.avatar_url(sender_user)?,
                is_direct: None,
                third_party_invite: None,
                blurhash: services().users.blurhash(sender_user)?,
                reason: reason.clone(),
                join_authorized_via_users_server,
            })
            .expect("event is valid, we just created it"),
        );

        member_event_stub.remove("event_id");

        ruma::signatures::hash_and_sign_event(
            services().globals.server_name().as_str(),
            services().globals.keypair(),
            &mut member_event_stub,
            &room_version_rules.redaction,
        )
        .expect("event is valid, we just created it");

        let event_id = format!(
            "${}",
            ruma::signatures::reference_hash(&member_event_stub, room_version_rules)
                .expect("Event format validated when event was hashed")
        );

        let event_id = <OwnedEventId>::try_from(event_id)
            .expect("ruma's reference hashes are valid event ids");

        member_event_stub.insert(
            "event_id".to_owned(),
            CanonicalJsonValue::String(event_id.as_str().to_owned()),
        );

        Ok((event_id, member_event_stub, restricted_join))
    }
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
    pub_key_map: &RwLock<BTreeMap<String, SigningKeys>>,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
    let mut value: CanonicalJsonObject = serde_json::from_str(pdu.get()).map_err(|e| {
        error!("Invalid PDU in server response: {:?}: {:?}", pdu, e);
        Error::BadServerResponse("Invalid PDU in server response")
    })?;
    let event_id = EventId::parse(format!(
        "${}",
        ruma::signatures::reference_hash(
            &value,
            &room_version
                .rules()
                .expect("Supported room version has rules")
        )
        .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid PDU format"))?
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

    let origin_server_ts = value.get("origin_server_ts").ok_or_else(|| {
        error!("Invalid PDU, no origin_server_ts field");
        Error::BadRequest(
            ErrorKind::MissingParam,
            "Invalid PDU, no origin_server_ts field",
        )
    })?;

    let origin_server_ts: MilliSecondsSinceUnixEpoch = {
        let ts = origin_server_ts.as_integer().ok_or_else(|| {
            Error::BadRequest(
                ErrorKind::InvalidParam,
                "origin_server_ts must be an integer",
            )
        })?;

        MilliSecondsSinceUnixEpoch(i64::from(ts).try_into().map_err(|_| {
            Error::BadRequest(ErrorKind::InvalidParam, "Time must be after the unix epoch")
        })?)
    };

    let unfiltered_keys = (*pub_key_map.read().await).clone();

    let keys =
        services()
            .globals
            .filter_keys_server_map(unfiltered_keys, origin_server_ts, room_version);

    if let Err(e) = ruma::signatures::verify_event(
        &keys,
        &value,
        &room_version
            .rules()
            .expect("Supported room version has rules"),
    ) {
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
