use std::{collections::HashSet, sync::Arc};

use crate::Result;
use ruma::{
    events::{AnyStrippedStateEvent, AnySyncStateEvent},
    serde::Raw,
    RoomId, ServerName, UserId,
};

pub trait Data: Send + Sync {
    fn mark_as_once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<()>;
    fn mark_as_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<()>;
    fn mark_as_invited(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
    ) -> Result<()>;
    fn mark_as_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<()>;

    fn update_joined_count(&self, room_id: &RoomId) -> Result<()>;

    fn get_our_real_users(&self, room_id: &RoomId) -> Result<Arc<HashSet<Box<UserId>>>>;

    fn appservice_in_room(
        &self,
        room_id: &RoomId,
        appservice: &(String, serde_yaml::Value),
    ) -> Result<bool>;

    /// Makes a user forget a room.
    fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()>;

    /// Returns an iterator of all servers participating in this room.
    fn room_servers<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<Box<ServerName>>> + 'a>;

    fn server_in_room<'a>(&'a self, server: &ServerName, room_id: &RoomId) -> Result<bool>;

    /// Returns an iterator of all rooms a server participates in (as far as we know).
    fn server_rooms<'a>(
        &'a self,
        server: &ServerName,
    ) -> Box<dyn Iterator<Item = Result<Box<RoomId>>> + 'a>;

    /// Returns an iterator over all joined members of a room.
    fn room_members<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<Box<UserId>>> + 'a>;

    fn room_joined_count(&self, room_id: &RoomId) -> Result<Option<u64>>;

    fn room_invited_count(&self, room_id: &RoomId) -> Result<Option<u64>>;

    /// Returns an iterator over all User IDs who ever joined a room.
    fn room_useroncejoined<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<Box<UserId>>> + 'a>;

    /// Returns an iterator over all invited members of a room.
    fn room_members_invited<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<Box<UserId>>> + 'a>;

    fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>>;

    fn get_left_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>>;

    /// Returns an iterator over all rooms this user joined.
    fn rooms_joined<'a>(
        &'a self,
        user_id: &UserId,
    ) -> Box<dyn Iterator<Item = Result<Box<RoomId>>> + 'a>;

    /// Returns an iterator over all rooms a user was invited to.
    fn rooms_invited<'a>(
        &'a self,
        user_id: &UserId,
    ) -> Box<dyn Iterator<Item = Result<(Box<RoomId>, Vec<Raw<AnyStrippedStateEvent>>)>> + 'a>;

    fn invite_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>>;

    fn left_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>>;

    /// Returns an iterator over all rooms a user left.
    fn rooms_left<'a>(
        &'a self,
        user_id: &UserId,
    ) -> Box<dyn Iterator<Item = Result<(Box<RoomId>, Vec<Raw<AnySyncStateEvent>>)>> + 'a>;

    fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool>;

    fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool>;

    fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool>;

    fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool>;
}
