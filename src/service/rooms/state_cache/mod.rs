mod data;
use std::{collections::HashSet, sync::Arc};

pub use data::Data;
use regex::Regex;
use ruma::{
    events::{
        direct::{DirectEvent, DirectEventContent},
        ignored_user_list::IgnoredUserListEvent,
        room::{create::RoomCreateEventContent, member::MembershipState},
        tag::{TagEvent, TagEventContent},
        AnyStrippedStateEvent, AnySyncStateEvent, GlobalAccountDataEventType,
        RoomAccountDataEventType, StateEventType, RoomAccountDataEvent, RoomAccountDataEventContent,
    },
    serde::Raw,
    RoomId, ServerName, UserId,
};

use crate::{services, utils, Error, Result};

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
                                serde_json::from_str(event.get())
                                    .map_err(|_| Error::bad_database("Invalid account data event in db."))
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
                        if let Some(mut direct_event) = services().account_data.get(
                            None,
                            user_id,
                            GlobalAccountDataEventType::Direct.to_string().into(),
                        )?
                            .map(|event| {
                                serde_json::from_str::<DirectEvent>(event.get())
                                    .map_err(|_| Error::bad_database("Invalid account data event in db."))
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
                                    &serde_json::to_value(&direct_event).expect("to json always works"),
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
                            }).transpose()?
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
        let mut joinedcount = 0_u64;
        let mut invitedcount = 0_u64;
        let mut joined_servers = HashSet::new();
        let mut real_users = HashSet::new();

        for joined in self.room_members(room_id).filter_map(|r| r.ok()) {
            joined_servers.insert(joined.server_name().to_owned());
            if joined.server_name() == services().globals.server_name()
                && !services().users.is_deactivated(&joined).unwrap_or(true)
            {
                real_users.insert(joined);
            }
            joinedcount += 1;
        }

        for invited in self.room_members_invited(room_id).filter_map(|r| r.ok()) {
            joined_servers.insert(invited.server_name().to_owned());
            invitedcount += 1;
        }

        self.roomid_joinedcount
            .insert(room_id.as_bytes(), &joinedcount.to_be_bytes())?;

        self.roomid_invitedcount
            .insert(room_id.as_bytes(), &invitedcount.to_be_bytes())?;

        self.our_real_users_cache
            .write()
            .unwrap()
            .insert(room_id.to_owned(), Arc::new(real_users));

        self.appservice_in_room_cache
            .write()
            .unwrap()
            .remove(room_id);

        Ok(())
    }

    #[tracing::instrument(skip(self, room_id))]
    pub fn get_our_real_users(&self, room_id: &RoomId) -> Result<Arc<HashSet<Box<UserId>>>> {
        let maybe = self
            .our_real_users_cache
            .read()
            .unwrap()
            .get(room_id)
            .cloned();
        if let Some(users) = maybe {
            Ok(users)
        } else {
            self.update_joined_count(room_id)?;
            Ok(Arc::clone(
                self.our_real_users_cache
                    .read()
                    .unwrap()
                    .get(room_id)
                    .unwrap(),
            ))
        }
    }

    #[tracing::instrument(skip(self, room_id, appservice))]
    pub fn appservice_in_room(
        &self,
        room_id: &RoomId,
        appservice: &(String, serde_yaml::Value),
    ) -> Result<bool> {
        let maybe = self
            .appservice_in_room_cache
            .read()
            .unwrap()
            .get(room_id)
            .and_then(|map| map.get(&appservice.0))
            .copied();

        if let Some(b) = maybe {
            Ok(b)
        } else if let Some(namespaces) = appservice.1.get("namespaces") {
            let users = namespaces
                .get("users")
                .and_then(|users| users.as_sequence())
                .map_or_else(Vec::new, |users| {
                    users
                        .iter()
                        .filter_map(|users| Regex::new(users.get("regex")?.as_str()?).ok())
                        .collect::<Vec<_>>()
                });

            let bridge_user_id = appservice
                .1
                .get("sender_localpart")
                .and_then(|string| string.as_str())
                .and_then(|string| {
                    UserId::parse_with_server_name(string, services().globals.server_name()).ok()
                });

            let in_room = bridge_user_id
                .map_or(false, |id| self.is_joined(&id, room_id).unwrap_or(false))
                || self.room_members(room_id).any(|userid| {
                    userid.map_or(false, |userid| {
                        users.iter().any(|r| r.is_match(userid.as_str()))
                    })
                });

            self.appservice_in_room_cache
                .write()
                .unwrap()
                .entry(room_id.to_owned())
                .or_default()
                .insert(appservice.0.clone(), in_room);

            Ok(in_room)
        } else {
            Ok(false)
        }
    }

    /// Makes a user forget a room.
    #[tracing::instrument(skip(self))]
    pub fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        self.userroomid_leftstate.remove(&userroom_id)?;
        self.roomuserid_leftcount.remove(&roomuser_id)?;

        Ok(())
    }

    /// Returns an iterator of all servers participating in this room.
    #[tracing::instrument(skip(self))]
    pub fn room_servers<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<Box<ServerName>>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomserverids.scan_prefix(prefix).map(|(key, _)| {
            ServerName::parse(
                utils::string_from_bytes(
                    key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| {
                    Error::bad_database("Server name in roomserverids is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("Server name in roomserverids is invalid."))
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn server_in_room<'a>(&'a self, server: &ServerName, room_id: &RoomId) -> Result<bool> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(room_id.as_bytes());

        self.serverroomids.get(&key).map(|o| o.is_some())
    }

    /// Returns an iterator of all rooms a server participates in (as far as we know).
    #[tracing::instrument(skip(self))]
    pub fn server_rooms<'a>(
        &'a self,
        server: &ServerName,
    ) -> impl Iterator<Item = Result<Box<RoomId>>> + 'a {
        let mut prefix = server.as_bytes().to_vec();
        prefix.push(0xff);

        self.serverroomids.scan_prefix(prefix).map(|(key, _)| {
            RoomId::parse(
                utils::string_from_bytes(
                    key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| Error::bad_database("RoomId in serverroomids is invalid unicode."))?,
            )
            .map_err(|_| Error::bad_database("RoomId in serverroomids is invalid."))
        })
    }

    /// Returns an iterator over all joined members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<Box<UserId>>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuserid_joined.scan_prefix(prefix).map(|(key, _)| {
            UserId::parse(
                utils::string_from_bytes(
                    key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| {
                    Error::bad_database("User ID in roomuserid_joined is invalid unicode.")
                })?,
            )
            .map_err(|_| Error::bad_database("User ID in roomuserid_joined is invalid."))
        })
    }

    #[tracing::instrument(skip(self))]
    pub fn room_joined_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.roomid_joinedcount
            .get(room_id.as_bytes())?
            .map(|b| {
                utils::u64_from_bytes(&b)
                    .map_err(|_| Error::bad_database("Invalid joinedcount in db."))
            })
            .transpose()
    }

    #[tracing::instrument(skip(self))]
    pub fn room_invited_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.roomid_invitedcount
            .get(room_id.as_bytes())?
            .map(|b| {
                utils::u64_from_bytes(&b)
                    .map_err(|_| Error::bad_database("Invalid joinedcount in db."))
            })
            .transpose()
    }

    /// Returns an iterator over all User IDs who ever joined a room.
    #[tracing::instrument(skip(self))]
    pub fn room_useroncejoined<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<Box<UserId>>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuseroncejoinedids
            .scan_prefix(prefix)
            .map(|(key, _)| {
                UserId::parse(
                    utils::string_from_bytes(
                        key.rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in room_useroncejoined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in room_useroncejoined is invalid."))
            })
    }

    /// Returns an iterator over all invited members of a room.
    #[tracing::instrument(skip(self))]
    pub fn room_members_invited<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> impl Iterator<Item = Result<Box<UserId>>> + 'a {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.roomuserid_invitecount
            .scan_prefix(prefix)
            .map(|(key, _)| {
                UserId::parse(
                    utils::string_from_bytes(
                        key.rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("User ID in roomuserid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("User ID in roomuserid_invited is invalid."))
            })
    }

    #[tracing::instrument(skip(self))]
    pub fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_invitecount
            .get(&key)?
            .map_or(Ok(None), |bytes| {
                Ok(Some(utils::u64_from_bytes(&bytes).map_err(|_| {
                    Error::bad_database("Invalid invitecount in db.")
                })?))
            })
    }

    #[tracing::instrument(skip(self))]
    pub fn get_left_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
        let mut key = room_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(user_id.as_bytes());

        self.roomuserid_leftcount
            .get(&key)?
            .map(|bytes| {
                utils::u64_from_bytes(&bytes)
                    .map_err(|_| Error::bad_database("Invalid leftcount in db."))
            })
            .transpose()
    }

    /// Returns an iterator over all rooms this user joined.
    #[tracing::instrument(skip(self))]
    pub fn rooms_joined<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<Box<RoomId>>> + 'a {
        self.userroomid_joined
            .scan_prefix(user_id.as_bytes().to_vec())
            .map(|(key, _)| {
                RoomId::parse(
                    utils::string_from_bytes(
                        key.rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_joined is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_joined is invalid."))
            })
    }

    /// Returns an iterator over all rooms a user was invited to.
    #[tracing::instrument(skip(self))]
    pub fn rooms_invited<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(Box<RoomId>, Vec<Raw<AnyStrippedStateEvent>>)>> + 'a {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.userroomid_invitestate
            .scan_prefix(prefix)
            .map(|(key, state)| {
                let room_id = RoomId::parse(
                    utils::string_from_bytes(
                        key.rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_invited is invalid."))?;

                let state = serde_json::from_slice(&state)
                    .map_err(|_| Error::bad_database("Invalid state in userroomid_invitestate."))?;

                Ok((room_id, state))
            })
    }

    #[tracing::instrument(skip(self))]
    pub fn invite_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(room_id.as_bytes());

        self.userroomid_invitestate
            .get(&key)?
            .map(|state| {
                let state = serde_json::from_slice(&state)
                    .map_err(|_| Error::bad_database("Invalid state in userroomid_invitestate."))?;

                Ok(state)
            })
            .transpose()
    }

    #[tracing::instrument(skip(self))]
    pub fn left_state(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
    ) -> Result<Option<Vec<Raw<AnyStrippedStateEvent>>>> {
        let mut key = user_id.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(room_id.as_bytes());

        self.userroomid_leftstate
            .get(&key)?
            .map(|state| {
                let state = serde_json::from_slice(&state)
                    .map_err(|_| Error::bad_database("Invalid state in userroomid_leftstate."))?;

                Ok(state)
            })
            .transpose()
    }

    /// Returns an iterator over all rooms a user left.
    #[tracing::instrument(skip(self))]
    pub fn rooms_left<'a>(
        &'a self,
        user_id: &UserId,
    ) -> impl Iterator<Item = Result<(Box<RoomId>, Vec<Raw<AnySyncStateEvent>>)>> + 'a {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        self.userroomid_leftstate
            .scan_prefix(prefix)
            .map(|(key, state)| {
                let room_id = RoomId::parse(
                    utils::string_from_bytes(
                        key.rsplit(|&b| b == 0xff)
                            .next()
                            .expect("rsplit always returns an element"),
                    )
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_invited is invalid unicode.")
                    })?,
                )
                .map_err(|_| Error::bad_database("Room ID in userroomid_invited is invalid."))?;

                let state = serde_json::from_slice(&state)
                    .map_err(|_| Error::bad_database("Invalid state in userroomid_leftstate."))?;

                Ok((room_id, state))
            })
    }

    #[tracing::instrument(skip(self))]
    pub fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.roomuseroncejoinedids.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    pub fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_joined.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    pub fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_invitestate.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    pub fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_leftstate.get(&userroom_id)?.is_some())
    }
}
