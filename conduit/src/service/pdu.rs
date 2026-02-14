use crate::Error;
use ruma::{
    CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId,
    OwnedRoomId, OwnedUserId, RoomId, UInt, UserId,
    api::client::error::ErrorKind,
    canonical_json::redact_content_in_place,
    events::{
        AnyInitialStateEvent, AnyMessageLikeEvent, AnyStateEvent, AnyStrippedStateEvent,
        AnySyncMessageLikeEvent, AnySyncStateEvent, AnySyncTimelineEvent, AnyTimelineEvent,
        StateEvent, TimelineEventType,
        room::{member::RoomMemberEventContent, redaction::RoomRedactionEventContent},
        space::child::HierarchySpaceChildEvent,
    },
    room_version_rules::{RedactionRules, RoomVersionRules},
    serde::{JsonCastable, Raw},
    state_res,
};
use serde::{Deserialize, Serialize};
use serde_json::{
    json,
    value::{RawValue as RawJsonValue, to_raw_value},
};
use std::{borrow::Cow, cmp::Ordering, collections::BTreeMap, sync::Arc};
use tracing::warn;

/// Content hashes of a PDU.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EventHash {
    /// The SHA-256 hash.
    pub sha256: String,
}

#[derive(Clone, Deserialize, Debug, Serialize)]
pub struct PduEvent {
    pub event_id: Arc<EventId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub room_id: Option<OwnedRoomId>,
    pub sender: OwnedUserId,
    pub origin_server_ts: UInt,
    #[serde(rename = "type")]
    pub kind: TimelineEventType,
    pub content: Box<RawJsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state_key: Option<String>,
    pub prev_events: Vec<Arc<EventId>>,
    pub depth: UInt,
    pub auth_events: Vec<Arc<EventId>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacts: Option<Arc<EventId>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unsigned: Option<Box<RawJsonValue>>,
    pub hashes: EventHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signatures: Option<Box<RawJsonValue>>, // BTreeMap<Box<ServerName>, BTreeMap<ServerSigningKeyId, String>>
}

impl PduEvent {
    #[tracing::instrument(skip(self))]
    pub fn redact(
        &mut self,
        redaction_rules: RedactionRules,
        reason: &PduEvent,
    ) -> crate::Result<()> {
        self.unsigned = None;

        let mut content = serde_json::from_str(self.content.get())
            .map_err(|_| Error::bad_database("PDU in db has invalid content."))?;
        redact_content_in_place(&mut content, &redaction_rules, self.kind.to_string())
            .map_err(|e| Error::RedactionError(self.sender.server_name().to_owned(), e))?;

        self.unsigned = Some(to_raw_value(&json!({
            "redacted_because": serde_json::to_value(reason).expect("to_value(PduEvent) always works")
        })).expect("to string always works"));

        self.content = to_raw_value(&content).expect("to string always works");

        Ok(())
    }

    pub fn is_redacted(&self) -> bool {
        #[derive(Deserialize)]
        struct ExtractRedactedBecause {
            redacted_because: Option<serde::de::IgnoredAny>,
        }

        let Some(unsigned) = &self.unsigned else {
            return false;
        };

        let Ok(unsigned) = ExtractRedactedBecause::deserialize(&**unsigned) else {
            return false;
        };

        unsigned.redacted_because.is_some()
    }

    pub fn remove_transaction_id(&mut self) -> crate::Result<()> {
        if let Some(unsigned) = &self.unsigned {
            let mut unsigned: BTreeMap<String, Box<RawJsonValue>> =
                serde_json::from_str(unsigned.get())
                    .map_err(|_| Error::bad_database("Invalid unsigned in pdu event"))?;
            unsigned.remove("transaction_id");
            self.unsigned = Some(to_raw_value(&unsigned).expect("unsigned is valid"));
        }

        Ok(())
    }

    pub fn add_age(&mut self) -> crate::Result<()> {
        let mut unsigned: BTreeMap<String, Box<RawJsonValue>> = self
            .unsigned
            .as_ref()
            .map_or_else(|| Ok(BTreeMap::new()), |u| serde_json::from_str(u.get()))
            .map_err(|_| Error::bad_database("Invalid unsigned in pdu event"))?;

        unsigned.insert("age".to_owned(), to_raw_value(&1).unwrap());
        self.unsigned = Some(to_raw_value(&unsigned).expect("unsigned is valid"));

        Ok(())
    }

    /// Copies the `redacts` property of the event to the `content` dict and vice-versa.
    ///
    /// This follows the specification's
    /// [recommendation](https://spec.matrix.org/v1.10/rooms/v11/#moving-the-redacts-property-of-mroomredaction-events-to-a-content-property):
    ///
    /// > For backwards-compatibility with older clients, servers should add a redacts
    /// > property to the top level of m.room.redaction events in when serving such events
    /// > over the Client-Server API.
    /// >
    /// > For improved compatibility with newer clients, servers should add a redacts property
    /// > to the content of m.room.redaction events in older room versions when serving
    /// > such events over the Client-Server API.
    pub fn copy_redacts(&self) -> (Option<Arc<EventId>>, Box<RawJsonValue>) {
        if self.kind == TimelineEventType::RoomRedaction {
            if let Ok(mut content) =
                serde_json::from_str::<RoomRedactionEventContent>(self.content.get())
            {
                if let Some(redacts) = content.redacts {
                    return (Some(redacts.into()), self.content.clone());
                } else if let Some(redacts) = self.redacts.clone() {
                    content.redacts = Some(redacts.into());
                    return (
                        self.redacts.clone(),
                        to_raw_value(&content).expect("Must be valid, we only added redacts field"),
                    );
                }
            }
        }

        (self.redacts.clone(), self.content.clone())
    }

    /// Gets the room id of the PDU.
    ///
    /// From `org.matrix.hydra.11`, this constructs the room ID from the event ID if it is an
    /// `m.room.create` event.
    pub fn room_id(&self) -> Cow<RoomId> {
        if let Some(room_id) = &self.room_id {
            Cow::Borrowed(room_id)
        } else {
            Cow::Owned(RoomId::new_v2(self.event_id.localpart()).expect(
                "Only create events from `org.matrix.hydra.11` onwards\
                    don't have a room id, and those events use the\
                    event id as reference hash format",
            ))
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn to_sync_room_event(&self) -> Raw<AnySyncTimelineEvent> {
        let (redacts, content) = self.copy_redacts();
        let mut json = json!({
            "content": content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
        });

        if let Some(unsigned) = &self.unsigned {
            json["unsigned"] = json!(unsigned);
        }
        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_sync_message_like_event(&self) -> Raw<AnySyncMessageLikeEvent> {
        let (redacts, content) = self.copy_redacts();
        let mut json = json!({
            "content": content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
        });

        if let Some(unsigned) = &self.unsigned {
            json["unsigned"] = json!(unsigned);
        }
        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_room_event(&self) -> Raw<AnyTimelineEvent> {
        let (redacts, content) = self.copy_redacts();
        let mut json = json!({
            "content": content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "room_id": self.room_id(),
        });

        if let Some(unsigned) = &self.unsigned {
            json["unsigned"] = json!(unsigned);
        }
        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_message_like_event(&self) -> Raw<AnyMessageLikeEvent> {
        let (redacts, content) = self.copy_redacts();
        let mut json = json!({
            "content": content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "room_id": self.room_id(),
        });

        if let Some(unsigned) = &self.unsigned {
            json["unsigned"] = json!(unsigned);
        }
        if let Some(state_key) = &self.state_key {
            json["state_key"] = json!(state_key);
        }
        if let Some(redacts) = &redacts {
            json["redacts"] = json!(redacts);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_state_event(&self) -> Raw<AnyStateEvent> {
        let mut json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "room_id": self.room_id(),
            "state_key": self.state_key,
        });

        if let Some(unsigned) = &self.unsigned {
            json["unsigned"] = json!(unsigned);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_sync_state_event(&self) -> Raw<AnySyncStateEvent> {
        let mut json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "state_key": self.state_key,
        });

        if let Some(unsigned) = &self.unsigned {
            json["unsigned"] = json!(unsigned);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
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
    pub fn to_stripped_spacechild_state_event(&self) -> Raw<HierarchySpaceChildEvent> {
        let json = json!({
            "content": self.content,
            "type": self.kind,
            "sender": self.sender,
            "state_key": self.state_key,
            "origin_server_ts": self.origin_server_ts,
        });

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    #[tracing::instrument(skip(self))]
    pub fn to_member_event(&self) -> Raw<StateEvent<RoomMemberEventContent>> {
        let mut json = json!({
            "content": self.content,
            "type": self.kind,
            "event_id": self.event_id,
            "sender": self.sender,
            "origin_server_ts": self.origin_server_ts,
            "redacts": self.redacts,
            "room_id": self.room_id(),
            "state_key": self.state_key,
        });

        if let Some(unsigned) = &self.unsigned {
            json["unsigned"] = json!(unsigned);
        }

        serde_json::from_value(json).expect("Raw::from_value always works")
    }

    /// This does not return a full `Pdu` it is only to satisfy ruma's types.
    #[tracing::instrument]
    pub fn convert_to_outgoing_federation_event(
        mut pdu_json: CanonicalJsonObject,
    ) -> Box<RawJsonValue> {
        if let Some(unsigned) = pdu_json
            .get_mut("unsigned")
            .and_then(|val| val.as_object_mut())
        {
            unsigned.remove("transaction_id");
        }

        pdu_json.remove("event_id");

        // TODO: another option would be to convert it to a canonical string to validate size
        // and return a Result<Raw<...>>
        // serde_json::from_str::<Raw<_>>(
        //     ruma::serde::to_canonical_json_string(pdu_json).expect("CanonicalJson is valid serde_json::Value"),
        // )
        // .expect("Raw::from_value always works")

        to_raw_value(&pdu_json).expect("CanonicalJson is valid serde_json::Value")
    }

    pub fn from_id_val(
        event_id: &EventId,
        mut json: CanonicalJsonObject,
    ) -> Result<Self, serde_json::Error> {
        json.insert(
            "event_id".to_owned(),
            CanonicalJsonValue::String(event_id.as_str().to_owned()),
        );

        serde_json::from_value(serde_json::to_value(json).expect("valid JSON"))
    }
}

impl state_res::Event for PduEvent {
    type Id = Arc<EventId>;

    fn event_id(&self) -> &Self::Id {
        &self.event_id
    }

    fn room_id(&self) -> Option<&RoomId> {
        self.room_id.as_deref()
    }

    fn sender(&self) -> &UserId {
        &self.sender
    }

    fn event_type(&self) -> &TimelineEventType {
        &self.kind
    }

    fn content(&self) -> &RawJsonValue {
        &self.content
    }

    fn origin_server_ts(&self) -> MilliSecondsSinceUnixEpoch {
        MilliSecondsSinceUnixEpoch(self.origin_server_ts)
    }

    fn state_key(&self) -> Option<&str> {
        self.state_key.as_deref()
    }

    fn prev_events(&self) -> Box<dyn DoubleEndedIterator<Item = &Self::Id> + '_> {
        Box::new(self.prev_events.iter())
    }

    fn auth_events(&self) -> Box<dyn DoubleEndedIterator<Item = &Self::Id> + '_> {
        Box::new(self.auth_events.iter())
    }

    fn redacts(&self) -> Option<&Self::Id> {
        self.redacts.as_ref()
    }

    // We currently don't store rejected events (see steps 6-8 of `handle_incoming_pdu`), even
    // though we should according to the spec:
    // https://spec.matrix.org/v1.14/rooms/v11/#rejected-events
    fn rejected(&self) -> bool {
        false
    }
}

// These impl's allow us to dedup state snapshots when resolving state
// for incoming events (federation/send/{txn}).
impl Eq for PduEvent {}
impl PartialEq for PduEvent {
    fn eq(&self, other: &Self) -> bool {
        self.event_id == other.event_id
    }
}
impl PartialOrd for PduEvent {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for PduEvent {
    fn cmp(&self, other: &Self) -> Ordering {
        self.event_id.cmp(&other.event_id)
    }
}

/// Generates a correct eventId for the incoming pdu.
///
/// Returns a tuple of the new `EventId` and the PDU as a `BTreeMap<String, CanonicalJsonValue>`.
pub(crate) fn gen_event_id_canonical_json(
    pdu: &RawJsonValue,
    room_version_rules: &RoomVersionRules,
) -> crate::Result<(OwnedEventId, CanonicalJsonObject)> {
    let value: CanonicalJsonObject = serde_json::from_str(pdu.get()).map_err(|e| {
        warn!("Error parsing incoming event {:?}: {:?}", pdu, e);
        Error::BadServerResponse("Invalid PDU in server response")
    })?;

    let event_id = format!(
        "${}",
        // Anything higher than version3 behaves the same
        ruma::signatures::reference_hash(&value, room_version_rules)
            .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid PDU format"))?
    )
    .try_into()
    .expect("ruma's reference hashes are valid event ids");

    Ok((event_id, value))
}

/// Build the start of a PDU in order to add it to the Database.
#[derive(Debug, Deserialize)]
pub struct PduBuilder {
    #[serde(rename = "type")]
    pub event_type: TimelineEventType,
    pub content: Box<RawJsonValue>,
    pub unsigned: Option<BTreeMap<String, serde_json::Value>>,
    pub state_key: Option<String>,
    pub redacts: Option<Arc<EventId>>,
    /// For timestamped messaging, should only be used for appservices
    ///
    /// Will be set to current time if None
    pub timestamp: Option<MilliSecondsSinceUnixEpoch>,
}

impl JsonCastable<PduBuilder> for AnyInitialStateEvent {}
