use crate::Result;
use ruma::{OwnedRoomAliasId, OwnedRoomId, OwnedUserId, RoomAliasId, RoomId, UserId};

pub trait Data: Send + Sync {
    /// Creates or updates the alias to the given room id.
    fn set_alias(&self, alias: &RoomAliasId, room_id: &RoomId, user_id: &UserId) -> Result<()>;

    /// Finds the user who assigned the given alias to a room
    fn who_created_alias(&self, alias: &RoomAliasId) -> Result<Option<OwnedUserId>>;

    /// Forgets about an alias. Returns an error if the alias did not exist.
    fn remove_alias(&self, alias: &RoomAliasId) -> Result<()>;

    /// Looks up the roomid for the given alias.
    fn resolve_local_alias(&self, alias: &RoomAliasId) -> Result<Option<OwnedRoomId>>;

    /// Returns all local aliases that point to the given room
    fn local_aliases_for_room<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<OwnedRoomAliasId>> + 'a>;
}
