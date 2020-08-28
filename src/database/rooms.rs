mod edus;

pub use edus::RoomEdus;

use crate::{pdu::PduBuilder, utils, Error, PduEvent, Result};
use log::error;
use ruma::{
    api::client::error::ErrorKind,
    events::{
        ignored_user_list,
        room::{
            join_rules, member,
            power_levels::{self, PowerLevelsEventContent},
        },
        EventType,
    },
    EventId, Raw, RoomAliasId, RoomId, UserId,
};
use sled::IVec;
use std::{
    collections::{BTreeMap, HashMap},
    convert::{TryFrom, TryInto},
    mem,
};

pub struct Rooms {
    pub edus: edus::RoomEdus,
    pub(super) pduid_pdu: sled::Tree, // PduId = RoomId + Count
    pub(super) eventid_pduid: sled::Tree,
    pub(super) roomid_pduleaves: sled::Tree,
    pub(super) roomstateid_pdu: sled::Tree, // RoomStateId = Room + StateType + StateKey

    pub(super) alias_roomid: sled::Tree,
    pub(super) aliasid_alias: sled::Tree, // AliasId = RoomId + Count
    pub(super) publicroomids: sled::Tree,

    pub(super) tokenids: sled::Tree, // TokenId = RoomId + Token + PduId

    pub(super) userroomid_joined: sled::Tree,
    pub(super) roomuserid_joined: sled::Tree,
    pub(super) userroomid_invited: sled::Tree,
    pub(super) roomuserid_invited: sled::Tree,
    pub(super) userroomid_left: sled::Tree,
}

impl Rooms {
    /// Checks if a room exists.
    pub fn exists(&self, room_id: &RoomId) -> Result<bool> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        // Look for PDUs in that room.
        Ok(self
            .pduid_pdu
            .get_gt(&prefix)?
            .filter(|(k, _)| k.starts_with(&prefix))
            .is_some())
    }

    /// Returns the full room state.
    pub fn room_state_full(
        &self,
        room_id: &RoomId,
    ) -> Result<HashMap<(EventType, String), PduEvent>> {
        let mut hashmap = HashMap::new();
        for pdu in self
            .roomstateid_pdu
            .scan_prefix(&room_id.to_string().as_bytes())
            .values()
            .map(|value| {
                Ok::<_, Error>(
                    serde_json::from_slice::<PduEvent>(&value?)
                        .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                )
            })
        {
            let pdu = pdu?;
            let state_key = pdu.state_key.clone().ok_or_else(|| {
                Error::bad_database("Room state contains event without state_key.")
            })?;
            hashmap.insert((pdu.kind.clone(), state_key), pdu);
        }
        Ok(hashmap)
    }

    /// Returns the all state entries for this type.
    pub fn room_state_type(
        &self,
        room_id: &RoomId,
        event_type: &EventType,
    ) -> Result<HashMap<String, PduEvent>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);
        prefix.extend_from_slice(&event_type.to_string().as_bytes());

        let mut hashmap = HashMap::new();
        for pdu in self
            .roomstateid_pdu
            .scan_prefix(&prefix)
            .values()
            .map(|value| {
                Ok::<_, Error>(
                    serde_json::from_slice::<PduEvent>(&value?)
                        .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                )
            })
        {
            let pdu = pdu?;
            let state_key = pdu.state_key.clone().ok_or_else(|| {
                Error::bad_database("Room state contains event without state_key.")
            })?;
            hashmap.insert(state_key, pdu);
        }
        Ok(hashmap)
    }

    /// Returns the full room state.
    pub fn room_state_get(
        &self,
        room_id: &RoomId,
        event_type: &EventType,
        state_key: &str,
    ) -> Result<Option<PduEvent>> {
        let mut key = room_id.to_string().as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(&event_type.to_string().as_bytes());
        key.push(0xff);
        key.extend_from_slice(&state_key.as_bytes());

        self.roomstateid_pdu.get(&key)?.map_or(Ok(None), |value| {
            Ok::<_, Error>(Some(
                serde_json::from_slice::<PduEvent>(&value)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Returns the `count` of this pdu's id.
    pub fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<u64>> {
        self.eventid_pduid
            .get(event_id.to_string().as_bytes())?
            .map_or(Ok(None), |pdu_id| {
                Ok(Some(
                    utils::u64_from_bytes(
                        &pdu_id[pdu_id.len() - mem::size_of::<u64>()..pdu_id.len()],
                    )
                    .map_err(|_| Error::bad_database("PDU has invalid count bytes."))?,
                ))
            })
    }

    /// Returns the json of a pdu.
    pub fn get_pdu_json(&self, event_id: &EventId) -> Result<Option<serde_json::Value>> {
        self.eventid_pduid
            .get(event_id.to_string().as_bytes())?
            .map_or(Ok(None), |pdu_id| {
                Ok(Some(
                    serde_json::from_slice(&self.pduid_pdu.get(pdu_id)?.ok_or_else(|| {
                        Error::bad_database("eventid_pduid points to nonexistent pdu.")
                    })?)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                ))
            })
    }

    /// Returns the pdu's id.
    pub fn get_pdu_id(&self, event_id: &EventId) -> Result<Option<IVec>> {
        self.eventid_pduid
            .get(event_id.to_string().as_bytes())?
            .map_or(Ok(None), |pdu_id| Ok(Some(pdu_id)))
    }

    /// Returns the pdu.
    pub fn get_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.eventid_pduid
            .get(event_id.to_string().as_bytes())?
            .map_or(Ok(None), |pdu_id| {
                Ok(Some(
                    serde_json::from_slice(&self.pduid_pdu.get(pdu_id)?.ok_or_else(|| {
                        Error::bad_database("eventid_pduid points to nonexistent pdu.")
                    })?)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
                ))
            })
    }
    /// Returns the pdu.
    pub fn get_pdu_from_id(&self, pdu_id: &IVec) -> Result<Option<PduEvent>> {
        self.pduid_pdu.get(pdu_id)?.map_or(Ok(None), |pdu| {
            Ok(Some(
                serde_json::from_slice(&pdu)
                    .map_err(|_| Error::bad_database("Invalid PDU in db."))?,
            ))
        })
    }

    /// Removes a pdu and creates a new one with the same id.
    fn replace_pdu(&self, pdu_id: &IVec, pdu: &PduEvent) -> Result<()> {
        if self.pduid_pdu.get(&pdu_id)?.is_some() {
            self.pduid_pdu.insert(
                &pdu_id,
                &*serde_json::to_string(pdu).expect("PduEvent::to_string always works"),
            )?;
            Ok(())
        } else {
            Err(Error::BadRequest(
                ErrorKind::NotFound,
                "PDU does not exist.",
            ))
        }
    }

    /// Returns the leaf pdus of a room.
    pub fn get_pdu_leaves(&self, room_id: &RoomId) -> Result<Vec<EventId>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut events = Vec::new();

        for event in self
            .roomid_pduleaves
            .scan_prefix(prefix)
            .values()
            .map(|bytes| {
                Ok::<_, Error>(
                    EventId::try_from(utils::string_from_bytes(&bytes?).map_err(|_| {
                        Error::bad_database("EventID in roomid_pduleaves is invalid unicode.")
                    })?)
                    .map_err(|_| Error::bad_database("EventId in roomid_pduleaves is invalid."))?,
                )
            })
        {
            events.push(event?);
        }

        Ok(events)
    }

    /// Replace the leaves of a room with a new event.
    pub fn replace_pdu_leaves(&self, room_id: &RoomId, event_id: &EventId) -> Result<()> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        for key in self.roomid_pduleaves.scan_prefix(&prefix).keys() {
            self.roomid_pduleaves.remove(key?)?;
        }

        prefix.extend_from_slice(event_id.to_string().as_bytes());
        self.roomid_pduleaves
            .insert(&prefix, &*event_id.to_string())?;

        Ok(())
    }

    /// Creates a new persisted data unit and adds it to a room.
    #[allow(clippy::blocks_in_if_conditions)]
    pub fn append_pdu(
        &self,
        pdu_builder: PduBuilder,
        globals: &super::globals::Globals,
        account_data: &super::account_data::AccountData,
    ) -> Result<EventId> {
        let PduBuilder {
            room_id,
            sender,
            event_type,
            content,
            unsigned,
            state_key,
            redacts,
        } = pdu_builder;
        // TODO: Make sure this isn't called twice in parallel
        let prev_events = self.get_pdu_leaves(&room_id)?;

        // Is the event authorized?
        if let Some(state_key) = &state_key {
            let power_levels = self
                .room_state_get(&room_id, &EventType::RoomPowerLevels, "")?
                .map_or_else(
                    || {
                        Ok::<_, Error>(power_levels::PowerLevelsEventContent {
                            ban: 50.into(),
                            events: BTreeMap::new(),
                            events_default: 0.into(),
                            invite: 50.into(),
                            kick: 50.into(),
                            redact: 50.into(),
                            state_default: 0.into(),
                            users: BTreeMap::new(),
                            users_default: 0.into(),
                            notifications:
                                ruma::events::room::power_levels::NotificationPowerLevels {
                                    room: 50.into(),
                                },
                        })
                    },
                    |power_levels| {
                        Ok(serde_json::from_value::<Raw<PowerLevelsEventContent>>(
                            power_levels.content,
                        )
                        .expect("Raw::from_value always works.")
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid PowerLevels event in db."))?)
                    },
                )?;
            let sender_membership = self
                .room_state_get(&room_id, &EventType::RoomMember, &sender.to_string())?
                .map_or(Ok::<_, Error>(member::MembershipState::Leave), |pdu| {
                    Ok(
                        serde_json::from_value::<Raw<member::MemberEventContent>>(pdu.content)
                            .expect("Raw::from_value always works.")
                            .deserialize()
                            .map_err(|_| Error::bad_database("Invalid Member event in db."))?
                            .membership,
                    )
                })?;

            let sender_power = power_levels.users.get(&sender).map_or_else(
                || {
                    if sender_membership != member::MembershipState::Join {
                        None
                    } else {
                        Some(&power_levels.users_default)
                    }
                },
                // If it's okay, wrap with Some(_)
                Some,
            );

            // Is the event allowed?
            if !match event_type {
                EventType::RoomEncryption => {
                    // Don't allow encryption events when it's disabled
                    !globals.encryption_disabled()
                }
                EventType::RoomMember => {
                    let target_user_id = UserId::try_from(&**state_key).map_err(|_| {
                        Error::BadRequest(
                            ErrorKind::InvalidParam,
                            "State key of member event does not contain user id.",
                        )
                    })?;

                    let current_membership = self
                        .room_state_get(
                            &room_id,
                            &EventType::RoomMember,
                            &target_user_id.to_string(),
                        )?
                        .map_or(Ok::<_, Error>(member::MembershipState::Leave), |pdu| {
                            Ok(serde_json::from_value::<Raw<member::MemberEventContent>>(
                                pdu.content,
                            )
                            .expect("Raw::from_value always works.")
                            .deserialize()
                            .map_err(|_| Error::bad_database("Invalid Member event in db."))?
                            .membership)
                        })?;

                    let target_membership =
                        serde_json::from_value::<Raw<member::MemberEventContent>>(content.clone())
                            .expect("Raw::from_value always works.")
                            .deserialize()
                            .map_err(|_| Error::bad_database("Invalid Member event in db."))?
                            .membership;

                    let target_power = power_levels.users.get(&target_user_id).map_or_else(
                        || {
                            if target_membership != member::MembershipState::Join {
                                None
                            } else {
                                Some(&power_levels.users_default)
                            }
                        },
                        // If it's okay, wrap with Some(_)
                        Some,
                    );

                    let join_rules =
                        self.room_state_get(&room_id, &EventType::RoomJoinRules, "")?
                            .map_or(Ok::<_, Error>(join_rules::JoinRule::Public), |pdu| {
                                Ok(serde_json::from_value::<
                                    Raw<join_rules::JoinRulesEventContent>,
                                >(pdu.content)
                                .expect("Raw::from_value always works.")
                                .deserialize()
                                .map_err(|_| {
                                    Error::bad_database("Database contains invalid JoinRules event")
                                })?
                                .join_rule)
                            })?;

                    if target_membership == member::MembershipState::Join {
                        let mut prev_events = prev_events.iter();
                        let prev_event = self
                            .get_pdu(prev_events.next().ok_or(Error::BadRequest(
                                ErrorKind::Unknown,
                                "Membership can't be the first event",
                            ))?)?
                            .ok_or_else(|| {
                                Error::bad_database("PDU leaf points to invalid event!")
                            })?;
                        if prev_event.kind == EventType::RoomCreate
                            && prev_event.prev_events.is_empty()
                        {
                            true
                        } else if sender != target_user_id {
                            false
                        } else if let member::MembershipState::Ban = current_membership {
                            false
                        } else {
                            join_rules == join_rules::JoinRule::Invite
                                && (current_membership == member::MembershipState::Join
                                    || current_membership == member::MembershipState::Invite)
                                || join_rules == join_rules::JoinRule::Public
                        }
                    } else if target_membership == member::MembershipState::Invite {
                        if let Some(third_party_invite_json) = content.get("third_party_invite") {
                            if current_membership == member::MembershipState::Ban {
                                false
                            } else {
                                let _third_party_invite =
                                    serde_json::from_value::<member::ThirdPartyInvite>(
                                        third_party_invite_json.clone(),
                                    )
                                    .map_err(|_| {
                                        Error::BadRequest(
                                            ErrorKind::InvalidParam,
                                            "ThirdPartyInvite is invalid",
                                        )
                                    })?;
                                todo!("handle third party invites");
                            }
                        } else if sender_membership != member::MembershipState::Join
                            || current_membership == member::MembershipState::Join
                            || current_membership == member::MembershipState::Ban
                        {
                            false
                        } else {
                            sender_power
                                .filter(|&p| p >= &power_levels.invite)
                                .is_some()
                        }
                    } else if target_membership == member::MembershipState::Leave {
                        if sender == target_user_id {
                            current_membership == member::MembershipState::Join
                                || current_membership == member::MembershipState::Invite
                        } else if sender_membership != member::MembershipState::Join
                            || current_membership == member::MembershipState::Ban
                                && sender_power.filter(|&p| p < &power_levels.ban).is_some()
                        {
                            false
                        } else {
                            sender_power.filter(|&p| p >= &power_levels.kick).is_some()
                                && target_power < sender_power
                        }
                    } else if target_membership == member::MembershipState::Ban {
                        if sender_membership != member::MembershipState::Join {
                            false
                        } else {
                            sender_power.filter(|&p| p >= &power_levels.ban).is_some()
                                && target_power < sender_power
                        }
                    } else {
                        false
                    }
                }
                EventType::RoomCreate => prev_events.is_empty(),
                // Not allow any of the following events if the sender is not joined.
                _ if sender_membership != member::MembershipState::Join => false,
                _ => {
                    // TODO
                    sender_power.unwrap_or(&power_levels.users_default)
                        >= &power_levels.state_default
                }
            } {
                error!("Unauthorized");
                // Not authorized
                return Err(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "Event is not authorized",
                ));
            }
        } else if !self.is_joined(&sender, &room_id)? {
            // TODO: auth rules apply to all events, not only those with a state key
            error!("Unauthorized");
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "Event is not authorized",
            ));
        }

        // Our depth is the maximum depth of prev_events + 1
        let depth = prev_events
            .iter()
            .filter_map(|event_id| Some(self.get_pdu_json(event_id).ok()??.get("depth")?.as_u64()?))
            .max()
            .unwrap_or(0_u64)
            + 1;

        let mut unsigned = unsigned.unwrap_or_default();
        if let Some(state_key) = &state_key {
            if let Some(prev_pdu) = self.room_state_get(&room_id, &event_type, &state_key)? {
                unsigned.insert("prev_content".to_owned(), prev_pdu.content);
                unsigned.insert(
                    "prev_sender".to_owned(),
                    serde_json::to_value(prev_pdu.sender).expect("UserId::to_value always works"),
                );
            }
        }

        let mut pdu = PduEvent {
            event_id: EventId::try_from("$thiswillbefilledinlater").expect("we know this is valid"),
            room_id: room_id.clone(),
            sender: sender.clone(),
            origin: globals.server_name().to_owned(),
            origin_server_ts: utils::millis_since_unix_epoch()
                .try_into()
                .expect("time is valid"),
            kind: event_type.clone(),
            content: content.clone(),
            state_key: state_key.clone(),
            prev_events,
            depth: depth
                .try_into()
                .map_err(|_| Error::bad_database("Depth is invalid"))?,
            auth_events: Vec::new(),
            redacts: redacts.clone(),
            unsigned,
            hashes: ruma::events::pdu::EventHash {
                sha256: "aaa".to_owned(),
            },
            signatures: HashMap::new(),
        };

        // Generate event id
        pdu.event_id = EventId::try_from(&*format!(
            "${}",
            ruma::signatures::reference_hash(
                &serde_json::to_value(&pdu).expect("event is valid, we just created it")
            )
            .expect("ruma can calculate reference hashes")
        ))
        .expect("ruma's reference hashes are valid event ids");

        let mut pdu_json = serde_json::to_value(&pdu).expect("event is valid, we just created it");
        ruma::signatures::hash_and_sign_event(
            globals.server_name().as_str(),
            globals.keypair(),
            &mut pdu_json,
        )
        .expect("event is valid, we just created it");

        self.replace_pdu_leaves(&room_id, &pdu.event_id)?;

        // Increment the last index and use that
        // This is also the next_batch/since value
        let index = globals.next_count()?;

        let mut pdu_id = room_id.to_string().as_bytes().to_vec();
        pdu_id.push(0xff);
        pdu_id.extend_from_slice(&index.to_be_bytes());

        self.pduid_pdu.insert(&pdu_id, &*pdu_json.to_string())?;

        self.eventid_pduid
            .insert(pdu.event_id.to_string(), pdu_id.clone())?;

        if let Some(state_key) = &pdu.state_key {
            let mut key = room_id.to_string().as_bytes().to_vec();
            key.push(0xff);
            key.extend_from_slice(pdu.kind.to_string().as_bytes());
            key.push(0xff);
            key.extend_from_slice(state_key.as_bytes());
            self.roomstateid_pdu.insert(key, &*pdu_json.to_string())?;
        }

        match event_type {
            EventType::RoomRedaction => {
                if let Some(redact_id) = &redacts {
                    self.redact_pdu(&redact_id, &pdu)?;
                }
            }
            EventType::RoomMember => {
                if let Some(state_key) = state_key {
                    // if the state_key fails
                    let target_user_id = UserId::try_from(state_key)
                        .expect("This state_key was previously validated");
                    // Update our membership info, we do this here incase a user is invited
                    // and immediately leaves we need the DB to record the invite event for auth
                    self.update_membership(
                        &room_id,
                        &target_user_id,
                        serde_json::from_value::<member::MemberEventContent>(content).map_err(
                            |_| {
                                Error::BadRequest(
                                    ErrorKind::InvalidParam,
                                    "Invalid redaction event content.",
                                )
                            },
                        )?,
                        &sender,
                        account_data,
                        globals,
                    )?;
                }
            }
            EventType::RoomMessage => {
                if let Some(body) = content.get("body").and_then(|b| b.as_str()) {
                    for word in body
                        .split_terminator(|c: char| !c.is_alphanumeric())
                        .map(str::to_lowercase)
                    {
                        let mut key = room_id.to_string().as_bytes().to_vec();
                        key.push(0xff);
                        key.extend_from_slice(word.as_bytes());
                        key.push(0xff);
                        key.extend_from_slice(&pdu_id);
                        self.tokenids.insert(key, &[])?;
                    }
                }
            }
            _ => {}
        }

        self.edus
            .private_read_set(&room_id, &sender, index, &globals)?;

        Ok(pdu.event_id)
    }

    /// Returns an iterator over all PDUs in a room.
    pub fn all_pdus(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<impl Iterator<Item = Result<PduEvent>>> {
        self.pdus_since(user_id, room_id, 0)
    }

    /// Returns a double-ended iterator over all events in a room that happened after the event with id `since`
    /// in chronological order.
    pub fn pdus_since(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        since: u64,
    ) -> Result<impl DoubleEndedIterator<Item = Result<PduEvent>>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        // Skip the first pdu if it's exactly at since, because we sent that last time
        let mut first_pdu_id = prefix.clone();
        first_pdu_id.extend_from_slice(&(since + 1).to_be_bytes());

        let mut last_pdu_id = prefix;
        last_pdu_id.extend_from_slice(&u64::MAX.to_be_bytes());

        let user_id = user_id.clone();
        Ok(self
            .pduid_pdu
            .range(first_pdu_id..last_pdu_id)
            .filter_map(|r| r.ok())
            .map(move |(_, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok(pdu)
            }))
    }

    /// Returns an iterator over all events and their tokens in a room that happened before the
    /// event with id `until` in reverse-chronological order.
    pub fn pdus_until(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        until: u64,
    ) -> impl Iterator<Item = Result<(u64, PduEvent)>> {
        // Create the first part of the full pdu id
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut current = prefix.clone();
        current.extend_from_slice(&until.to_be_bytes());

        let current: &[u8] = &current;

        let user_id = user_id.clone();
        let prefixlen = prefix.len();
        self.pduid_pdu
            .range(..current)
            .rev()
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(k, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((
                    utils::u64_from_bytes(&k[prefixlen..])
                        .map_err(|_| Error::bad_database("Invalid pdu id in db."))?,
                    pdu,
                ))
            })
    }

    /// Returns an iterator over all events and their token in a room that happened after the event
    /// with id `from` in chronological order.
    pub fn pdus_after(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        from: u64,
    ) -> impl Iterator<Item = Result<(u64, PduEvent)>> {
        // Create the first part of the full pdu id
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let mut current = prefix.clone();
        current.extend_from_slice(&(from + 1).to_be_bytes()); // +1 so we don't send the base event

        let current: &[u8] = &current;

        let user_id = user_id.clone();
        let prefixlen = prefix.len();
        self.pduid_pdu
            .range(current..)
            .filter_map(|r| r.ok())
            .take_while(move |(k, _)| k.starts_with(&prefix))
            .map(move |(k, v)| {
                let mut pdu = serde_json::from_slice::<PduEvent>(&v)
                    .map_err(|_| Error::bad_database("PDU in db is invalid."))?;
                if pdu.sender != user_id {
                    pdu.unsigned.remove("transaction_id");
                }
                Ok((
                    utils::u64_from_bytes(&k[prefixlen..])
                        .map_err(|_| Error::bad_database("Invalid pdu id in db."))?,
                    pdu,
                ))
            })
    }

    /// Replace a PDU with the redacted form.
    pub fn redact_pdu(&self, event_id: &EventId, reason: &PduEvent) -> Result<()> {
        if let Some(pdu_id) = self.get_pdu_id(event_id)? {
            let mut pdu = self
                .get_pdu_from_id(&pdu_id)?
                .ok_or_else(|| Error::bad_database("PDU ID points to invalid PDU."))?;
            pdu.redact(&reason)?;
            self.replace_pdu(&pdu_id, &pdu)?;
            Ok(())
        } else {
            Err(Error::BadRequest(
                ErrorKind::NotFound,
                "Event ID does not exist.",
            ))
        }
    }

    /// Update current membership data.
    fn update_membership(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        mut member_content: member::MemberEventContent,
        sender: &UserId,
        account_data: &super::account_data::AccountData,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        let membership = member_content.membership;
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        let mut roomuser_id = room_id.to_string().as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.to_string().as_bytes());

        match &membership {
            member::MembershipState::Join => {
                self.userroomid_joined.insert(&userroom_id, &[])?;
                self.roomuserid_joined.insert(&roomuser_id, &[])?;
                self.userroomid_invited.remove(&userroom_id)?;
                self.roomuserid_invited.remove(&roomuser_id)?;
                self.userroomid_left.remove(&userroom_id)?;
            }
            member::MembershipState::Invite => {
                // We want to know if the sender is ignored by the receiver
                let is_ignored = account_data
                    .get::<ignored_user_list::IgnoredUserListEvent>(
                        None,     // Ignored users are in global account data
                        &user_id, // Receiver
                        EventType::IgnoredUserList,
                    )?
                    .map_or(false, |ignored| {
                        ignored.content.ignored_users.contains(&sender)
                    });

                if is_ignored {
                    member_content.membership = member::MembershipState::Leave;

                    self.append_pdu(
                        PduBuilder {
                            room_id: room_id.clone(),
                            sender: user_id.clone(),
                            event_type: EventType::RoomMember,
                            content: serde_json::to_value(member_content)
                                .expect("event is valid, we just created it"),
                            unsigned: None,
                            state_key: Some(user_id.to_string()),
                            redacts: None,
                        },
                        globals,
                        account_data,
                    )?;

                    return Ok(());
                }
                self.userroomid_invited.insert(&userroom_id, &[])?;
                self.roomuserid_invited.insert(&roomuser_id, &[])?;
                self.userroomid_joined.remove(&userroom_id)?;
                self.roomuserid_joined.remove(&roomuser_id)?;
                self.userroomid_left.remove(&userroom_id)?;
            }
            member::MembershipState::Leave | member::MembershipState::Ban => {
                self.userroomid_left.insert(&userroom_id, &[])?;
                self.userroomid_joined.remove(&userroom_id)?;
                self.roomuserid_joined.remove(&roomuser_id)?;
                self.userroomid_invited.remove(&userroom_id)?;
                self.roomuserid_invited.remove(&roomuser_id)?;
            }
            _ => {}
        }

        Ok(())
    }

    /// Makes a user forget a room.
    pub fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()> {
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        self.userroomid_left.remove(userroom_id)?;

        Ok(())
    }

    pub fn set_alias(
        &self,
        alias: &RoomAliasId,
        room_id: Option<&RoomId>,
        globals: &super::globals::Globals,
    ) -> Result<()> {
        if let Some(room_id) = room_id {
            // New alias
            self.alias_roomid
                .insert(alias.alias(), &*room_id.to_string())?;
            let mut aliasid = room_id.to_string().as_bytes().to_vec();
            aliasid.extend_from_slice(&globals.next_count()?.to_be_bytes());
            self.aliasid_alias.insert(aliasid, &*alias.alias())?;
        } else {
            // room_id=None means remove alias
            let room_id = self
                .alias_roomid
                .remove(alias.alias())?
                .ok_or(Error::BadRequest(
                    ErrorKind::NotFound,
                    "Alias does not exist.",
                ))?;

            for key in self.aliasid_alias.scan_prefix(room_id).keys() {
                self.aliasid_alias.remove(key?)?;
            }
        }

        Ok(())
    }

    pub fn id_from_alias(&self, alias: &RoomAliasId) -> Result<Option<RoomId>> {
        self.alias_roomid
            .get(alias.alias())?
            .map_or(Ok(None), |bytes| {
                Ok(Some(
                    RoomId::try_from(utils::string_from_bytes(&bytes).map_err(|_| {
                        Error::bad_database("Room ID in alias_roomid is invalid unicode.")
                    })?)
                    .map_err(|_| Error::bad_database("Room ID in alias_roomid is invalid."))?,
                ))
            })
    }

    pub fn room_aliases(&self, room_id: &RoomId) -> impl Iterator<Item = Result<RoomAliasId>> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        self.aliasid_alias
            .scan_prefix(prefix)
            .values()
            .map(|bytes| {
                Ok(serde_json::from_slice(&bytes?)
                    .map_err(|_| Error::bad_database("Alias in aliasid_alias is invalid."))?)
            })
    }

    pub fn set_public(&self, room_id: &RoomId, public: bool) -> Result<()> {
        if public {
            self.publicroomids.insert(room_id.to_string(), &[])?;
        } else {
            self.publicroomids.remove(room_id.to_string())?;
        }

        Ok(())
    }

    pub fn is_public_room(&self, room_id: &RoomId) -> Result<bool> {
        Ok(self.publicroomids.contains_key(room_id.to_string())?)
    }

    pub fn public_rooms(&self) -> impl Iterator<Item = Result<RoomId>> {
        self.publicroomids.iter().keys().map(|bytes| {
            Ok(
                RoomId::try_from(utils::string_from_bytes(&bytes?).map_err(|_| {
                    Error::bad_database("Room ID in publicroomids is invalid unicode.")
                })?)
                .map_err(|_| Error::bad_database("Room ID in publicroomids is invalid."))?,
            )
        })
    }

    pub fn search_pdus<'a>(
        &'a self,
        room_id: &RoomId,
        search_string: &str,
    ) -> Result<(impl Iterator<Item = IVec> + 'a, Vec<String>)> {
        let mut prefix = room_id.to_string().as_bytes().to_vec();
        prefix.push(0xff);

        let words = search_string
            .split_terminator(|c: char| !c.is_alphanumeric())
            .map(str::to_lowercase)
            .collect::<Vec<_>>();

        let iterators = words.clone().into_iter().map(move |word| {
            let mut prefix2 = prefix.clone();
            prefix2.extend_from_slice(word.as_bytes());
            prefix2.push(0xff);
            self.tokenids
                .scan_prefix(&prefix2)
                .keys()
                .rev() // Newest pdus first
                .filter_map(|r| r.ok())
                .map(|key| {
                    let pduid_index = key
                        .iter()
                        .enumerate()
                        .filter(|(_, &b)| b == 0xff)
                        .nth(1)
                        .ok_or_else(|| Error::bad_database("Invalid tokenid in db."))?
                        .0
                        + 1; // +1 because the pdu id starts AFTER the separator

                    let pdu_id = key.subslice(pduid_index, key.len() - pduid_index);

                    Ok::<_, Error>(pdu_id)
                })
                .filter_map(|r| r.ok())
        });

        Ok((
            utils::common_elements(iterators, |a, b| {
                // We compare b with a because we reversed the iterator earlier
                b.cmp(a)
            })
            .unwrap(),
            words,
        ))
    }

    pub fn get_shared_rooms<'a>(
        &'a self,
        users: Vec<UserId>,
    ) -> impl Iterator<Item = Result<RoomId>> + 'a {
        let iterators = users.into_iter().map(move |user_id| {
            let mut prefix = user_id.as_bytes().to_vec();
            prefix.push(0xff);

            self.userroomid_joined
                .scan_prefix(&prefix)
                .keys()
                .filter_map(|r| r.ok())
                .map(|key| {
                    let roomid_index = key
                        .iter()
                        .enumerate()
                        .filter(|(_, &b)| b == 0xff)
                        .nth(0)
                        .ok_or_else(|| Error::bad_database("Invalid userroomid_joined in db."))?
                        .0
                        + 1; // +1 because the room id starts AFTER the separator

                    let room_id = key.subslice(roomid_index, key.len() - roomid_index);

                    Ok::<_, Error>(room_id)
                })
                .filter_map(|r| r.ok())
        });

        // We use the default compare function because keys are sorted correctly (not reversed)
        utils::common_elements(iterators, Ord::cmp)
            .expect("users is not empty")
            .map(|bytes| {
                RoomId::try_from(utils::string_from_bytes(&*bytes).map_err(|_| {
                    Error::bad_database("Invalid RoomId bytes in userroomid_joined")
                })?)
                .map_err(|_| Error::bad_database("Invalid RoomId in userroomid_joined."))
            })
    }

    /// Returns an iterator over all joined members of a room.
    pub fn room_members(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        self.roomuserid_joined
            .scan_prefix(room_id.to_string())
            .keys()
            .map(|key| {
                Ok(UserId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in roomuserid_joined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in roomuserid_joined is invalid."))?)
            })
    }

    /// Returns an iterator over all invited members of a room.
    pub fn room_members_invited(&self, room_id: &RoomId) -> impl Iterator<Item = Result<UserId>> {
        self.roomuserid_invited
            .scan_prefix(room_id.to_string())
            .keys()
            .map(|key| {
                Ok(UserId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in roomuserid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in roomuserid_invited is invalid."))?)
            })
    }

    /// Returns an iterator over all rooms this user joined.
    pub fn rooms_joined(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        self.userroomid_joined
            .scan_prefix(user_id.to_string())
            .keys()
            .map(|key| {
                Ok(RoomId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_joined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_joined is invalid."))?)
            })
    }

    /// Returns an iterator over all rooms a user was invited to.
    pub fn rooms_invited(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        self.userroomid_invited
            .scan_prefix(&user_id.to_string())
            .keys()
            .map(|key| {
                Ok(RoomId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_invited is invalid."))?)
            })
    }

    /// Returns an iterator over all rooms a user left.
    pub fn rooms_left(&self, user_id: &UserId) -> impl Iterator<Item = Result<RoomId>> {
        self.userroomid_left
            .scan_prefix(&user_id.to_string())
            .keys()
            .map(|key| {
                Ok(RoomId::try_from(
                    utils::string_from_bytes(
                        &key?
                            .rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_left is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_left is invalid."))?)
            })
    }

    pub fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        Ok(self.userroomid_joined.get(userroom_id)?.is_some())
    }

    pub fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        Ok(self.userroomid_invited.get(userroom_id)?.is_some())
    }

    pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.to_string().as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.to_string().as_bytes());

        Ok(self.userroomid_left.get(userroom_id)?.is_some())
    }
}
