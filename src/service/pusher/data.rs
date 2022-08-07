pub trait Data {
    fn set_pusher(&self, sender: &UserId, pusher: set_pusher::v3::Pusher) -> Result<()>;

    pub fn get_pusher(&self, senderkey: &[u8]) -> Result<Option<get_pushers::v3::Pusher>>;

    pub fn get_pushers(&self, sender: &UserId) -> Result<Vec<get_pushers::v3::Pusher>>;

    pub fn get_pusher_senderkeys<'a>(
        &'a self,
        sender: &UserId,
    ) -> impl Iterator<Item = Vec<u8>> + 'a;
}
