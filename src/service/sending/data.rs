use ruma::ServerName;

use crate::Result;

use super::{OutgoingKind, SendingEventType};

pub trait Data: Send + Sync {
    fn active_requests<'a>(
        &'a self,
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, OutgoingKind, SendingEventType)>> + 'a>;
    fn active_requests_for<'a>(
        &'a self,
        outgoing_kind: &OutgoingKind,
    ) -> Box<dyn Iterator<Item = Result<(Vec<u8>, SendingEventType)>> + 'a>;
    fn delete_active_request(&self, key: Vec<u8>) -> Result<()>;
    fn delete_all_active_requests_for(&self, outgoing_kind: &OutgoingKind) -> Result<()>;
    fn delete_all_requests_for(&self, outgoing_kind: &OutgoingKind) -> Result<()>;
    fn queue_requests(
        &self,
        requests: &[(&OutgoingKind, SendingEventType)],
    ) -> Result<Vec<Vec<u8>>>;
    fn queued_requests<'a>(
        &'a self,
        outgoing_kind: &OutgoingKind,
    ) -> Box<dyn Iterator<Item = Result<(SendingEventType, Vec<u8>)>> + 'a>;
    fn mark_as_active(&self, events: &[(SendingEventType, Vec<u8>)]) -> Result<()>;
    fn set_latest_educount(&self, server_name: &ServerName, educount: u64) -> Result<()>;
    fn get_latest_educount(&self, server_name: &ServerName) -> Result<u64>;
}
