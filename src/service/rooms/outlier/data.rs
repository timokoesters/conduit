use ruma::{signatures::CanonicalJsonObject, EventId};

use crate::{PduEvent, Result};

pub trait Data: Send + Sync {
    fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>>;
    fn get_outlier_pdu(&self, event_id: &EventId) -> Result<Option<PduEvent>>;
    fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) -> Result<()>;
}
