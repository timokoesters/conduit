use crate::Result;
use ruma::{
    api::client::push::{set_pusher, Pusher},
    UserId,
};

pub trait Data: Send + Sync {
    fn set_pusher(&self, sender: &UserId, pusher: set_pusher::v3::PusherAction) -> Result<()>;

    fn get_pusher(&self, sender: &UserId, pushkey: &str) -> Result<Option<Pusher>>;

    fn get_pushers(&self, sender: &UserId) -> Result<Vec<Pusher>>;

    fn get_pushkeys<'a>(&'a self, sender: &UserId)
        -> Box<dyn Iterator<Item = Result<String>> + 'a>;
}
