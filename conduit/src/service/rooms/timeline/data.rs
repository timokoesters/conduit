use std::sync::Arc;

use ruma::{CanonicalJsonObject, EventId, OwnedUserId, RoomId, UserId};

use crate::{PduEvent, Result};

use super::PduCount;

pub trait Data: Send + Sync {
    fn last_timeline_count(&self, sender_user: &UserId, room_id: &RoomId) -> Result<PduCount>;

    /// Returns the `count` of this pdu's id.
    fn get_pdu_count(&self, event_id: &EventId) -> Result<Option<PduCount>>;

    /// Returns the json of a pdu.
    fn get_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>>;

    /// Returns the json of a pdu.
    fn get_non_outlier_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>>;

    /// Returns the pdu's id.
    fn get_pdu_id(&self, event_id: &EventId) -> Result<Option<Vec<u8>>>;

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    fn get_non_outlier_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>>;

    /// Returns the pdu.
    ///
    /// Checks the `eventid_outlierpdu` Tree if not found in the timeline.
    fn get_pdu(&self, event_id: &EventId) -> Result<Option<Arc<PduEvent>>>;

    /// Returns the pdu.
    ///
    /// This does __NOT__ check the outliers `Tree`.
    fn get_pdu_from_id(&self, pdu_id: &[u8]) -> Result<Option<PduEvent>>;

    /// Returns the pdu as a `BTreeMap<String, CanonicalJsonValue>`.
    fn get_pdu_json_from_id(&self, pdu_id: &[u8]) -> Result<Option<CanonicalJsonObject>>;

    /// Adds a new pdu to the timeline
    fn append_pdu(
        &self,
        pdu_id: &[u8],
        pdu: &PduEvent,
        json: &CanonicalJsonObject,
        count: u64,
    ) -> Result<()>;

    // Adds a new pdu to the backfilled timeline
    fn prepend_backfill_pdu(
        &self,
        pdu_id: &[u8],
        event_id: &EventId,
        json: &CanonicalJsonObject,
    ) -> Result<()>;

    /// Removes a pdu and creates a new one with the same id.
    fn replace_pdu(
        &self,
        pdu_id: &[u8],
        pdu_json: &CanonicalJsonObject,
        pdu: &PduEvent,
    ) -> Result<()>;

    /// Returns an iterator over all events and their tokens in a room that happened before the
    /// event with id `until` in reverse-chronological order.
    #[allow(clippy::type_complexity)]
    fn pdus_until<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        until: PduCount,
    ) -> Result<Box<dyn Iterator<Item = Result<(PduCount, PduEvent)>> + 'a>>;

    /// Returns an iterator over all events in a room that happened after the event with id `from`
    /// in chronological order.
    #[allow(clippy::type_complexity)]
    fn pdus_after<'a>(
        &'a self,
        user_id: &UserId,
        room_id: &RoomId,
        from: PduCount,
    ) -> Result<Box<dyn Iterator<Item = Result<(PduCount, PduEvent)>> + 'a>>;

    fn increment_notification_counts(
        &self,
        room_id: &RoomId,
        notifies: Vec<OwnedUserId>,
        highlights: Vec<OwnedUserId>,
    ) -> Result<()>;
}
