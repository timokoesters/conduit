use ruma::{UserId, api::client::push::{set_pusher, get_pushers}};
use crate::Result;

pub trait Data {
    fn set_pusher(&self, sender: &UserId, pusher: set_pusher::v3::Pusher) -> Result<()>;

    fn get_pusher(&self, senderkey: &[u8]) -> Result<Option<get_pushers::v3::Pusher>>;

    fn get_pushers(&self, sender: &UserId) -> Result<Vec<get_pushers::v3::Pusher>>;

    fn get_pusher_senderkeys<'a>(
        &'a self,
        sender: &UserId,
    ) -> Box<dyn Iterator<Item = Vec<u8>>>;
}
