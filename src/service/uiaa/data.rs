use ruma::{api::client::uiaa::UiaaInfo, DeviceId, UserId, signatures::CanonicalJsonValue};
use crate::Result;

pub trait Data: Send + Sync {
    fn set_uiaa_request(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
        request: &CanonicalJsonValue,
    ) -> Result<()>;

    fn get_uiaa_request(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
    ) -> Option<CanonicalJsonValue>;

    fn update_uiaa_session(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
        uiaainfo: Option<&UiaaInfo>,
    ) -> Result<()>;

    fn get_uiaa_session(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
    ) -> Result<UiaaInfo>;
}
