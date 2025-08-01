mod data;
use std::{collections::HashSet, sync::Arc};

pub use data::Data;

use ruma::{
    api::client::sync::sync_events::StrippedState,
    events::{
        direct::DirectEvent,
        ignored_user_list::IgnoredUserListEvent,
        room::{create::RoomCreateEventContent, member::MembershipState},
        AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    serde::Raw,
    OwnedRoomId, OwnedRoomOrAliasId, OwnedServerName, OwnedUserId, RoomId, ServerName, UserId,
};
use tracing::warn;

use crate::{service::appservice::RegistrationInfo, services, Error, Result};

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    /// Update current membership data.
    #[tracing::instrument(skip(self, last_state))]
    pub fn update_membership(
        &self,
        room_id: &RoomId,
        user_id: &UserId,
        membership: MembershipState,
        sender: &UserId,
        last_state: Option<Vec<Raw<StrippedState>>>,
        update_joined_count: bool,
    ) -> Result<()> {
        // Keep track what remote users exist by adding them as "deactivated" users
        if user_id.server_name() != services().globals.server_name() {
            services().users.create(user_id, None)?;
            // TODO: displayname, avatar url
        }

        // We don't need to store stripped state on behalf of remote users, since these events are only used on `/sync`
        let last_state = if user_id.server_name() == services().globals.server_name() {
            last_state
        } else {
            None
        };

        match &membership {
            MembershipState::Join => {
                // Check if the user never joined this room
                if !self.once_joined(user_id, room_id)? {
                    // Add the user ID to the join list then
                    self.db.mark_as_once_joined(user_id, room_id)?;

                    // Check if the room has a predecessor
                    if let Some(predecessor) = services()
                        .rooms
                        .state_accessor
                        .room_state_get(room_id, &StateEventType::RoomCreate, "")?
                        .and_then(|create| serde_json::from_str(create.content.get()).ok())
                        .and_then(|content: RoomCreateEventContent| content.predecessor)
                    {
                        // Copy user settings from predecessor to the current room:
                        // - Push rules
                        //
                        // TODO: finish this once push rules are implemented.
                        //
                        // let mut push_rules_event_content: PushRulesEvent = account_data
                        //     .get(
                        //         None,
                        //         user_id,
                        //         EventType::PushRules,
                        //     )?;
                        //
                        // NOTE: find where `predecessor.room_id` match
                        //       and update to `room_id`.
                        //
                        // account_data
                        //     .update(
                        //         None,
                        //         user_id,
                        //         EventType::PushRules,
                        //         &push_rules_event_content,
                        //         globals,
                        //     )
                        //     .ok();

                        // Copy old tags to new room
                        if let Some(tag_event) = services()
                            .account_data
                            .get(
                                Some(&predecessor.room_id),
                                user_id,
                                RoomAccountDataEventType::Tag,
                            )?
                            .map(|event| {
                                serde_json::from_str(event.get()).map_err(|e| {
                                    warn!("Invalid account data event in db: {e:?}");
                                    Error::BadDatabase("Invalid account data event in db.")
                                })
                            })
                        {
                            services()
                                .account_data
                                .update(
                                    Some(room_id),
                                    user_id,
                                    RoomAccountDataEventType::Tag,
                                    &tag_event?,
                                )
                                .ok();
                        };

                        // Copy direct chat flag
                        if let Some(direct_event) = services()
                            .account_data
                            .get(
                                None,
                                user_id,
                                GlobalAccountDataEventType::Direct.to_string().into(),
                            )?
                            .map(|event| {
                                serde_json::from_str::<DirectEvent>(event.get()).map_err(|e| {
                                    warn!("Invalid account data event in db: {e:?}");
                                    Error::BadDatabase("Invalid account data event in db.")
                                })
                            })
                        {
                            let mut direct_event = direct_event?;
                            let mut room_ids_updated = false;

                            for room_ids in direct_event.content.0.values_mut() {
                                if room_ids.iter().any(|r| r == &predecessor.room_id) {
                                    room_ids.push(room_id.to_owned());
                                    room_ids_updated = true;
                                }
                            }

                            if room_ids_updated {
                                services().account_data.update(
                                    None,
                                    user_id,
                                    GlobalAccountDataEventType::Direct.to_string().into(),
                                    &serde_json::to_value(&direct_event)
                                        .expect("to json always works"),
                                )?;
                            }
                        };
                    }
                }

                self.db.mark_as_joined(user_id, room_id)?;
            }
            MembershipState::Invite => {
                // We want to know if the sender is ignored by the receiver
                let is_ignored = services()
                    .account_data
                    .get(
                        None,    // Ignored users are in global account data
                        user_id, // Receiver
                        GlobalAccountDataEventType::IgnoredUserList
                            .to_string()
                            .into(),
                    )?
                    .map(|event| {
                        serde_json::from_str::<IgnoredUserListEvent>(event.get()).map_err(|e| {
                            warn!("Invalid account data event in db: {e:?}");
                            Error::BadDatabase("Invalid account data event in db.")
                        })
                    })
                    .transpose()?
                    .is_some_and(|ignored| {
                        ignored
                            .content
                            .ignored_users
                            .iter()
                            .any(|(user, _details)| user == sender)
                    });

                if is_ignored {
                    return Ok(());
                }

                self.db.mark_as_invited(user_id, room_id, last_state)?;
            }
            MembershipState::Knock => {
                self.db.mark_as_knocked(user_id, room_id, last_state)?;
            }
            MembershipState::Leave | MembershipState::Ban => {
                self.db.mark_as_left(user_id, room_id)?;
            }
            _ => {}
        }

        if update_joined_count {
            self.update_joined_count(room_id)?;
        }

        Ok(())
    }

    #[tracing::instrument(skip(self, room_id))]
    pub fn update_joined_count(&self, room_id: &RoomId) -> Result<()> {
        self.db.update_joined_count(room_id)
    }

    #[tracing::instrument(skip(self, room_id))]
    pub fn get_our_real_users(&self, room_id: &RoomId) -> Result<Arc<HashSet<OwnedUserId>>> {
        self.db.get_our_real_users(room_id)
    }

    #[tracing::instrument(skip(self, room_id, appservice))]
    pub fn appservice_in_room(
        &self,
        room_id: &RoomId,
        appservice: &RegistrationInfo,
    ) -> Result<bool> {
        self.db.appservice_in_room(room_id, appservice)
    }

    /// Makes a user forget a room.
    #[tracing::instrument(skip(self))]
    pub fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()> {
        self.db.forget(room_id, user_id)
    }

    /// Returns an iterator of all servers participating in this room.
    #[tracing::instrument(skip(self))]
    pub fn room_servers<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<OwnedServerName>> + 'a {
        self.db.room_servers(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn server_in_room(&self, server: &ServerName, room_id: &RoomId) -> Result<bool> {
        self.db.server_in_room(server, room_id)
    }

    /// Returns an iterator of all rooms a server participates in (as far as we know).
    #[tracing::instrument(skip(self))]
    pub fn server_rooms<'a>(
        &'a self,
        server: &ServerName,
    ) -> impl Iterator<Item = Result<OwnedRoomId>> + 'a {
        self.db.server_rooms(server)
    }

    /// Returns an iterator over all joined members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<OwnedUserId>> + 'a {
        self.db.room_members(room_id)
    }

    /// Returns the number of users which are currently in a room
    #[tracing::instrument(skip(self))]
    pub fn room_joined_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.db.room_joined_count(room_id)
    }

    /// Returns the number of users which are currently invited to a room
    #[tracing::instrument(skip(self))]
    pub fn room_invited_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.db.room_invited_count(room_id)
    }

    /// Returns an iterator over all User IDs who ever joined a room.
    #[tracing::instrument(skip(self))]
    pub fn room_useroncejoined<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<OwnedUserId>> + 'a {
        self.db.room_useroncejoined(room_id)
    }

    /// Returns an iterator over all invited members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members_invited<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<OwnedUserId>> + 'a {
        self.db.room_members_invited(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        self.db.get_invite_count(room_id, user_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn get_knock_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        self.db.get_knock_count(room_id, user_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn get_left_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        self.db.get_left_count(room_id, user_id)
    }

    /// Returns an iterator over all rooms this user joined.
    #[tracing::instrument(skip(self))]
    pub fn rooms_joined<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<OwnedRoomId>> + 'a {
        self.db.rooms_joined(user_id)
    }

    /// Returns an iterator over all rooms a user was invited to.
    #[tracing::instrument(skip(self))]
    pub fn rooms_invited<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(OwnedRoomId, Vec<Raw<StrippedState>>)>> + 'a {
        self.db.rooms_invited(user_id)
    }

    /// Returns an iterator over all rooms a user has knocked on.
    #[tracing::instrument(skip(self))]
    pub fn rooms_knocked<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(OwnedRoomId, Vec<Raw<StrippedState>>)>> + 'a {
        self.db.rooms_knocked(user_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn invite_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<StrippedState>>>> {
        self.db.invite_state(user_id, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn knock_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<StrippedState>>>> {
        self.db.knock_state(user_id, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn left_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<StrippedState>>>> {
        self.db.left_state(user_id, room_id)
    }

    /// Returns an iterator over all rooms a user left.
    #[tracing::instrument(skip(self))]
    pub fn rooms_left<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(OwnedRoomId, Vec<Raw<AnySyncStateEvent>>)>> + 'a {
        self.db.rooms_left(user_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        self.db.once_joined(user_id, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        self.db.is_joined(user_id, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        self.db.is_invited(user_id, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_knocked(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        self.db.is_knocked(user_id, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        self.db.is_left(user_id, room_id)
    }

    /// Function to assist performing a membership event that may require help from a remote server
    ///
    /// If a room id is provided, the servers returned will consist of:
    /// - the `via` argument, provided by the client
    /// - servers of the senders of the stripped state events we are given
    /// - the server in the room id
    ///
    /// Otherwise, the servers returned will come from the response when resolving the alias.
    #[tracing::instrument(skip(self))]
    pub async fn get_room_id_and_via_servers(
        &self,
        sender_user: &UserId,
        room_id_or_alias: OwnedRoomOrAliasId,
        via: Vec<OwnedServerName>,
    ) -> Result<(Vec<OwnedServerName>, OwnedRoomId), Error> {
        let (servers, room_id) = match OwnedRoomId::try_from(room_id_or_alias) {
            Ok(room_id) => {
                let mut servers = via;
                servers.extend(
                    self.invite_state(sender_user, &room_id)
                        .transpose()
                        .or_else(|| self.knock_state(sender_user, &room_id).transpose())
                        .transpose()?
                        .unwrap_or_default()
                        .iter()
                        .filter_map(|event| event.deserialize().ok())
                        .map(|event| event.sender().server_name().to_owned()),
                );

                if let Some(server_name) = room_id.server_name() {
                    servers.push(server_name.to_owned())
                };

                (servers, room_id)
            }
            Err(room_alias) => {
                let response = services().rooms.alias.get_alias_helper(room_alias).await?;

                (response.servers, response.room_id)
            }
        };
        Ok((servers, room_id))
    }
}
