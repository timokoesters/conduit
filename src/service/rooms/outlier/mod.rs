mod data;
use std::sync::Arc;

pub use data::Data;
use ruma::{EventId, signatures::CanonicalJsonObject};

use crate::{Result, PduEvent};

pub struct Service {
    db: Arc<dyn Data>,
}

impl Service {
    /// Returns the pdu from the outlier tree.
    pub fn get_outlier_pdu_json(&self, event_id: &EventId) -> Result<Option<CanonicalJsonObject>> {
        self.db.get_outlier_pdu_json(event_id)
    }

    /// Returns the pdu from the outlier tree.
    pub fn get_pdu_outlier(&self, event_id: &EventId) -> Result<Option<PduEvent>> {
        self.db.get_outlier_pdu(event_id)
    }

    /// Append the PDU as an outlier.
    #[tracing::instrument(skip(self, pdu))]
    pub fn add_pdu_outlier(&self, event_id: &EventId, pdu: &CanonicalJsonObject) -> Result<()> {
        self.db.add_pdu_outlier(event_id, pdu)
    }
}
