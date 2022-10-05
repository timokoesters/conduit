mod data;
use std::{collections::HashSet, sync::Arc};

pub use data::Data;

use ruma::{
    events::{
        direct::{DirectEvent},
        ignored_user_list::IgnoredUserListEvent,
        room::{create::RoomCreateEventContent, member::MembershipState},
        AnyStrippedStateEvent, AnySyncStateEvent, GlobalAccountDataEventType, RoomAccountDataEventType, StateEventType,
    },
    serde::Raw,
    RoomId, ServerName, UserId,
};

use crate::{services, Error, Result};

pub struct Service {
    db: Arc<dyn Data>,
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
        last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
        update_joined_count: bool,
    ) -> Result<()> {
        // Keep track what remote users exist by adding them as "deactivated" users
        if user_id.server_name() != services().globals.server_name() {
            services().users.create(user_id, None)?;
            // TODO: displayname, avatar url
        }

        let mut roomserver_id = room_id.as_bytes().to_vec();
        roomserver_id.push(0xff);
        roomserver_id.extend_from_slice(user_id.server_name().as_bytes());

        let mut serverroom_id = user_id.server_name().as_bytes().to_vec();
        serverroom_id.push(0xff);
        serverroom_id.extend_from_slice(room_id.as_bytes());

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
                                serde_json::from_str(event.get()).map_err(|_| {
                                    Error::bad_database("Invalid account data event in db.")
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
                        if let Some(mut direct_event) = services()
                            .account_data
                            .get(
                                None,
                                user_id,
                                GlobalAccountDataEventType::Direct.to_string().into(),
                            )?
                            .map(|event| {
                                serde_json::from_str::<DirectEvent>(event.get()).map_err(|_| {
                                    Error::bad_database("Invalid account data event in db.")
                                })
                            })
                        {
                            let direct_event = direct_event?;
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
                        serde_json::from_str::<IgnoredUserListEvent>(event.get())
                            .map_err(|_| Error::bad_database("Invalid account data event in db."))
                    })
                    .transpose()?
                    .map_or(false, |ignored| {
                        ignored
                            .content
                            .ignored_users
                            .iter()
                            .any(|user| user == sender)
                    });

                if is_ignored {
                    return Ok(());
                }

                self.db.mark_as_invited(user_id, room_id, last_state)?;
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
    pub fn get_our_real_users(&self, room_id: &RoomId) -> Result<Arc<HashSet<Box<UserId>>>> {
        self.db.get_our_real_users(room_id)
    }

    #[tracing::instrument(skip(self, room_id, appservice))]
    pub fn appservice_in_room(
        &self,
        room_id: &RoomId,
        appservice: &(String, serde_yaml::Value),
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
    ) -> impl Iterator<Item = Result<Box<ServerName>>> + 'a {
        self.db.room_servers(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn server_in_room<'a>(&'a self, server: &ServerName, room_id: &RoomId) -> Result<bool> {
        self.db.server_in_room(server, room_id)
    }

    /// Returns an iterator of all rooms a server participates in (as far as we know).
    #[tracing::instrument(skip(self))]
    pub fn server_rooms<'a>(
        &'a self,
        server: &ServerName,
    ) -> impl Iterator<Item = Result<Box<RoomId>>> + 'a {
        self.db.server_rooms(server)
    }

    /// Returns an iterator over all joined members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<Box<UserId>>> + 'a {
        self.db.room_members(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn room_joined_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.db.room_joined_count(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn room_invited_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.db.room_invited_count(room_id)
    }

    /// Returns an iterator over all User IDs who ever joined a room.
    #[tracing::instrument(skip(self))]
    pub fn room_useroncejoined<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<Box<UserId>>> + 'a {
        self.db.room_useroncejoined(room_id)
    }

    /// Returns an iterator over all invited members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members_invited<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<Box<UserId>>> + 'a {
        self.db.room_members_invited(room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        self.db.get_invite_count(room_id, user_id)
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
    ) -> impl Iterator<Item = Result<Box<RoomId>>> + 'a {
        self.db.rooms_joined(user_id)
    }

    /// Returns an iterator over all rooms a user was invited to.
    #[tracing::instrument(skip(self))]
    pub fn rooms_invited<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(Box<RoomId>, Vec<Raw<AnyStrippedStateEvent>>)>> + 'a {
        self.db.rooms_invited(user_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn invite_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>> {
        self.db.invite_state(user_id, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn left_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>> {
        self.db.left_state(user_id, room_id)
    }

    /// Returns an iterator over all rooms a user left.
    #[tracing::instrument(skip(self))]
    pub fn rooms_left<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(Box<RoomId>, Vec<Raw<AnySyncStateEvent>>)>> + 'a {
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
    pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        self.db.is_left(user_id, room_id)
    }
}
