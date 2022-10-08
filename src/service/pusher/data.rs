use crate::Result;
use ruma::{
    api::client::push::{get_pushers, set_pusher},
    UserId,
};

pub trait Data: Send + Sync {
    fn set_pusher(&self, sender: &UserId, pusher: set_pusher::v3::Pusher) -> Result<()>;

    fn get_pusher(&self, sender: &UserId, pushkey: &str) -> Result<Option<get_pushers::v3::Pusher>>;

    fn get_pushers(&self, sender: &UserId) -> Result<Vec<get_pushers::v3::Pusher>>;

    fn get_pushkeys<'a>(&'a self, sender: &UserId) -> Box<dyn Iterator<Item = Result<String>> + 'a>;
}
