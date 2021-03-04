use crate::Error;
use ruma::{
    events::{
        pdu::EventHash, room::member::MemberEventContent, AnyEvent, AnyRoomEvent, AnyStateEvent,
        AnyStrippedStateEvent, AnySyncRoomEvent, AnySyncStateEvent, EventType, StateEvent,
    },
    serde::{to_canonical_value, CanonicalJsonObject, CanonicalJsonValue, Raw},
    EventId, RoomId, RoomVersionId, ServerName, ServerSigningKeyId, UInt, UserId,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{
    collections::BTreeMap,
    convert::{TryFrom, TryInto},
    sync::Arc,
    time::UNIX_EPOCH,
};

#[derive(Deserialize, Serialize, Debug)]
pub struct PduEvent {
    pub event_id: EventId,
    pub room_id: RoomId,
    pub sender: UserId,
    pub origin_server_ts: UInt,
    #[serde(rename = "type")]
    pub kind: EventType,
    pub content: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_key: Option<String>,
    pub prev_events: Vec<EventId>,
    pub depth: UInt,
    pub auth_events: Vec<EventId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacts: Option<EventId>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub unsigned: serde_json::Map<String, serde_json::Value>,
    pub hashes: EventHash,
    pub signatures: BTreeMap<Box<ServerName>, BTreeMap<ServerSigningKeyId, String>>,
}

impl PduEvent {
    #[tracing::instrument(skip(self))]
    pub fn redact(&mut self, reason: &PduEvent) -> crate::Result<()> {
        self.unsigned.clear();

        let allowed: &[&str] = match self.kind {
            EventType::RoomMember => &["membership"],
            EventType::RoomCreate => &["creator"],
            EventType::RoomJoinRules => &["join_rule"],
            EventType::RoomPowerLevels => &[
                "ban",
                "events",
                "events_default",
                "kick",
                "redact",
                "state_default",
                "users",
                "users_default",
            ],
            EventType::RoomHistoryVisibility => &["history_visibility"],
            _ => &[],
        };

        let old_content = self
            .content
            .as_object_mut()
            .ok_or_else(|| Error::bad_database("PDU in db has invalid content."))?;

        let mut new_content = serde_json::Map::new();

        for key in allowed {
            if let Some(value) = old_content.remove(*key) {
                new_content.insert((*key).to_owned(), value);
            }
        }

        self.unsigned.insert(
            "redacted_because".to_owned(),
            serde_json::to_string(reason)
                .expect("PduEvent::to_string always works")
                .into(),
        );

        self.content = new_content.into();

        Ok(())
    }

    #[tracing::instrument(skip(self))]
    pub fn to_sync_room_event(&self) -> Raw<AnySyncRoomEvent> {
        let mut json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "unsigned": self.unsigned,
        });

        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &self.redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    /// This only works for events that are also AnyRoomEvents.
    #[tracing::instrument(skip(self))]
    pub fn to_any_event(&self) -> Raw<AnyEvent> {
        let mut json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "unsigned": self.unsigned,
            "room_id": self.room_id,
        });

        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &self.redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_room_event(&self) -> Raw<AnyRoomEvent> {
        let mut json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "unsigned": self.unsigned,
            "room_id": self.room_id,
        });

        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &self.redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_state_event(&self) -> Raw<AnyStateEvent> {
        let json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "unsigned": self.unsigned,
            "room_id": self.room_id,
            "state_key": self.state_key,
        });

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_sync_state_event(&self) -> Raw<AnySyncStateEvent> {
        let json = format!(
            r#"{{"content":{},"type":"{}","event_id":"{}","sender":"{}","origin_server_ts":{},"unsigned":{},"state_key":"{}"}}"#,
            self.content,
            self.kind,
            self.event_id,
            self.sender,
            self.origin_server_ts,
            serde_json::to_string(&self.unsigned).expect("Map::to_string always works"),
            self.state_key
                .as_ref()
                .expect("state events have state keys")
        );

        Raw::from_json(
            serde_json::value::RawValue::from_string(json).expect("our string is valid json"),
        )
    }

    #[tracing::instrument(skip(self))]
    pub fn to_stripped_state_event(&self) -> Raw<AnyStrippedStateEvent> {
        let json = json!({
            "content": self.content,
            "type": self.kind,
            "sender": self.sender,
            "state_key": self.state_key,
        });

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_member_event(&self) -> Raw<StateEvent<MemberEventContent>> {
        let json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "redacts": self.redacts,
            "unsigned": self.unsigned,
            "room_id": self.room_id,
            "state_key": self.state_key,
        });

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    /// This does not return a full `Pdu` it is only to satisfy ruma's types.
    #[tracing::instrument]
    pub fn convert_to_outgoing_federation_event(
        mut pdu_json: CanonicalJsonObject,
    ) -> Raw<ruma::events::pdu::Pdu> {
        if let Some(CanonicalJsonValue::Object(unsigned)) = pdu_json.get_mut("unsigned") {
            unsigned.remove("transaction_id");
        }

        pdu_json.remove("event_id");

        // TODO: another option would be to convert it to a canonical string to validate size
        // and return a Result<Raw<...>>
        // serde_json::from_str::<Raw<_>>(
        //     ruma::serde::to_canonical_json_string(pdu_json).expect("CanonicalJson is valid serde_json::Value"),
        // )
        // .expect("Raw::from_value always works")

        serde_json::from_value::<Raw<_>>(
            serde_json::to_value(pdu_json).expect("CanonicalJson is valid serde_json::Value"),
        )
        .expect("Raw::from_value always works")
    }
}

impl From<&state_res::StateEvent> for PduEvent {
    fn from(pdu: &state_res::StateEvent) -> Self {
        Self {
            event_id: pdu.event_id(),
            room_id: pdu.room_id().clone(),
            sender: pdu.sender().clone(),
            origin_server_ts: (pdu
                .origin_server_ts()
                .duration_since(UNIX_EPOCH)
                .expect("time is valid")
                .as_millis() as u64)
                .try_into()
                .expect("time is valid"),
            kind: pdu.kind(),
            content: pdu.content().clone(),
            state_key: Some(pdu.state_key()),
            prev_events: pdu.prev_event_ids(),
            depth: *pdu.depth(),
            auth_events: pdu.auth_events(),
            redacts: pdu.redacts().cloned(),
            unsigned: pdu.unsigned().clone().into_iter().collect(),
            hashes: pdu.hashes().clone(),
            signatures: pdu.signatures(),
        }
    }
}

impl PduEvent {
    pub fn convert_for_state_res(&self) -> Arc<state_res::StateEvent> {
        Arc::new(
            // For consistency of eventId (just in case) we use the one
            // generated by conduit for everything.
            state_res::StateEvent::from_id_value(
                self.event_id.clone(),
                json!({
                    "event_id": self.event_id,
                    "room_id": self.room_id,
                    "sender": self.sender,
                    "origin_server_ts": self.origin_server_ts,
                    "type": self.kind,
                    "content": self.content,
                    "state_key": self.state_key,
                    "prev_events": self.prev_events,
                    "depth": self.depth,
                    "auth_events": self.auth_events,
                    "redacts": self.redacts,
                    "unsigned": self.unsigned,
                    "hashes": self.hashes,
                    "signatures": self.signatures,
                }),
            )
            .expect("all conduit PDUs are state events"),
        )
    }
}

/// Generates a correct eventId for the incoming pdu.
///
/// Returns a tuple of the new `EventId` and the PDU with the eventId inserted as a `serde_json::Value`.
pub(crate) fn process_incoming_pdu(
    pdu: &Raw<ruma::events::pdu::Pdu>,
) -> (EventId, CanonicalJsonObject) {
    let mut value =
        serde_json::from_str(pdu.json().get()).expect("A Raw<...> is always valid JSON");

    let event_id = EventId::try_from(&*format!(
        "${}",
        ruma::signatures::reference_hash(&value, &RoomVersionId::Version6)
            .expect("ruma can calculate reference hashes")
    ))
    .expect("ruma's reference hashes are valid event ids");

    value.insert(
        "event_id".to_owned(),
        to_canonical_value(&event_id).expect("EventId is a valid CanonicalJsonValue"),
    );

    (event_id, value)
}

/// Build the start of a PDU in order to add it to the `Database`.
#[derive(Debug, Deserialize)]
pub struct PduBuilder {
    #[serde(rename = "type")]
    pub event_type: EventType,
    pub content: serde_json::Value,
    pub unsigned: Option<serde_json::Map<String, serde_json::Value>>,
    pub state_key: Option<String>,
    pub redacts: Option<EventId>,
}
