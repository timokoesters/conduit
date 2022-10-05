use ruma::{RoomId, RoomAliasId};
use crate::Result;

pub trait Data {
    /// Creates or updates the alias to the given room id.
    fn set_alias(
        &self,
        alias: &RoomAliasId,
        room_id: &RoomId
    ) -> Result<()>;

    /// Forgets about an alias. Returns an error if the alias did not exist.
    fn remove_alias(
        &self,
        alias: &RoomAliasId,
    ) -> Result<()>;

    /// Looks up the roomid for the given alias.
    fn resolve_local_alias(
        &self,
        alias: &RoomAliasId,
    ) -> Result<Option<Box<RoomId>>>;

    /// Returns all local aliases that point to the given room
    fn local_aliases_for_room(
        &self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<Box<RoomAliasId>>>>;
}
