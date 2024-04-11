mod data;
use std::{collections::HashSet, sync::Arc};

pub use data::Data;

use itertools::Itertools;
use ruma::{
    events::{
        direct::DirectEvent,
        ignored_user_list::IgnoredUserListEvent,
        room::{
            create::RoomCreateEventContent, member::MembershipState,
            power_levels::RoomPowerLevelsEventContent,
        },
        AnyStrippedStateEvent, AnySyncStateEvent, GlobalAccountDataEventType,
        RoomAccountDataEventType, StateEventType,
    },
    int,
    serde::Raw,
    OwnedRoomId, OwnedServerName, OwnedUserId, RoomId, ServerName, UserId,
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
        last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
        invite_via: Option<Vec<OwnedServerName>>,
        update_joined_count: bool,
    ) -> Result<()> {
        // Keep track what remote users exist by adding them as "deactivated" users
        if user_id.server_name() != services().globals.server_name() {
            services().users.create(user_id, None)?;
            // TODO: displayname, avatar url
        }

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
                    .map_or(false, |ignored| {
                        ignored
                            .content
                            .ignored_users
                            .iter()
                            .any(|(user, _details)| user == sender)
                    });

                if is_ignored {
                    return Ok(());
                }

                self.db
                    .mark_as_invited(user_id, room_id, last_state, invite_via)?;
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
    pub fn server_in_room<'a>(&'a self, server: &ServerName, room_id: &RoomId) -> Result<bool> {
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
    ) -> impl Iterator<Item = Result<(OwnedRoomId, Vec<Raw<AnyStrippedStateEvent>>)>> + 'a {
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
    pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        self.db.is_left(user_id, room_id)
    }

    #[tracing::instrument(skip(self))]
    pub fn servers_invite_via(&self, room_id: &RoomId) -> Result<Option<Vec<OwnedServerName>>> {
        self.db.servers_invite_via(room_id)
    }

    /// Gets up to three servers that are likely to be in the room in the distant future.
    ///
    /// See https://spec.matrix.org/v1.10/appendices/#routing
    #[tracing::instrument(skip(self))]
    pub fn servers_route_via(&self, room_id: &RoomId) -> Result<Vec<OwnedServerName>> {
        let most_powerful_user_server = services()
            .rooms
            .state_accessor
            .room_state_get(room_id, &StateEventType::RoomPowerLevels, "")?
            .map(|pdu| {
                serde_json::from_str(pdu.content.get()).map(
                    |conent: RoomPowerLevelsEventContent| {
                        conent
                            .users
                            .iter()
                            .max_by_key(|(_, power)| power)
                            .and_then(|x| if x.1 >= &int!(50) { Some(x) } else { None })
                            .map(|(user, power)| user.server_name().to_owned())
                    },
                )
            })
            .transpose()
            .map_err(|e| Error::bad_database("Invalid power levels event content in database"))?
            .flatten();

        let mut servers = services()
            .rooms
            .state_cache
            .room_members(room_id)
            .filter_map(Result::ok)
            .counts_by(|user| user.server_name())
            .iter()
            .sorted_by_key(|(_, users)| users)
            .map(|(server, _)| server.to_owned().to_owned())
            .rev()
            .take(3)
            .collect_vec();

        if let Some(server) = most_powerful_user_server {
            servers.insert(0, server);
            servers.truncate(3);
        }
        Ok(servers)
    }
}
