use std::{collections::HashSet, sync::Arc};

use regex::Regex;
use ruma::{
    events::{AnyStrippedStateEvent, AnySyncStateEvent},
    serde::Raw,
    OwnedRoomId, OwnedServerName, OwnedUserId, RoomId, ServerName, UserId,
};

use crate::{database::KeyValueDatabase, service, services, utils, Error, Result};

impl service::rooms::state_cache::Data for KeyValueDatabase {
    fn mark_as_once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());
        self.roomuseroncejoinedids.insert(&userroom_id, &[])
    }

    fn mark_as_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_joined.insert(&userroom_id, &[])?;
        self.roomuserid_joined.insert(&roomuser_id, &[])?;
        self.userroomid_invitestate.remove(&userroom_id)?;
        self.roomuserid_invitecount.remove(&roomuser_id)?;
        self.userroomid_leftstate.remove(&userroom_id)?;
        self.roomuserid_leftcount.remove(&roomuser_id)?;

        Ok(())
    }

    fn mark_as_invited(
        &self,
        user_id: &UserId,
        room_id: &RoomId,
        last_state: Option<Vec<Raw<AnyStrippedStateEvent>>>,
    ) -> Result<()> {
        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_invitestate.insert(
            &userroom_id,
            &serde_json::to_vec(&last_state.unwrap_or_default())
                .expect("state to bytes always works"),
        )?;
        self.roomuserid_invitecount.insert(
            &roomuser_id,
            &services().globals.next_count()?.to_be_bytes(),
        )?;
        self.userroomid_joined.remove(&userroom_id)?;
        self.roomuserid_joined.remove(&roomuser_id)?;
        self.userroomid_leftstate.remove(&userroom_id)?;
        self.roomuserid_leftcount.remove(&roomuser_id)?;

        Ok(())
    }

    fn mark_as_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<()> {
        let mut roomuser_id = room_id.as_bytes().to_vec();
        roomuser_id.push(0xff);
        roomuser_id.extend_from_slice(user_id.as_bytes());

        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        self.userroomid_leftstate.insert(
            &userroom_id,
            &serde_json::to_vec(&Vec::<Raw<AnySyncStateEvent>>::new()).unwrap(),
        )?; // TODO
        self.roomuserid_leftcount.insert(
            &roomuser_id,
            &services().globals.next_count()?.to_be_bytes(),
        )?;
        self.userroomid_joined.remove(&userroom_id)?;
        self.roomuserid_joined.remove(&roomuser_id)?;
        self.userroomid_invitestate.remove(&userroom_id)?;
        self.roomuserid_invitecount.remove(&roomuser_id)?;

        Ok(())
    }

    fn update_joined_count(&self, room_id: &RoomId) -> Result<()> {
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

        for _invited in self.room_members_invited(room_id).filter_map(|r| r.ok()) {
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

        for old_joined_server in self.room_servers(room_id).filter_map(|r| r.ok()) {
            if !joined_servers.remove(&old_joined_server) {
                // Server not in room anymore
                let mut roomserver_id = room_id.as_bytes().to_vec();
                roomserver_id.push(0xff);
                roomserver_id.extend_from_slice(old_joined_server.as_bytes());

                let mut serverroom_id = old_joined_server.as_bytes().to_vec();
                serverroom_id.push(0xff);
                serverroom_id.extend_from_slice(room_id.as_bytes());

                self.roomserverids.remove(&roomserver_id)?;
                self.serverroomids.remove(&serverroom_id)?;
            }
        }

        // Now only new servers are in joined_servers anymore
        for server in joined_servers {
            let mut roomserver_id = room_id.as_bytes().to_vec();
            roomserver_id.push(0xff);
            roomserver_id.extend_from_slice(server.as_bytes());

            let mut serverroom_id = server.as_bytes().to_vec();
            serverroom_id.push(0xff);
            serverroom_id.extend_from_slice(room_id.as_bytes());

            self.roomserverids.insert(&roomserver_id, &[])?;
            self.serverroomids.insert(&serverroom_id, &[])?;
        }

        self.appservice_in_room_cache
            .write()
            .unwrap()
            .remove(room_id);

        Ok(())
    }

    #[tracing::instrument(skip(self, room_id))]
    fn get_our_real_users(&self, room_id: &RoomId) -> Result<Arc<HashSet<OwnedUserId>>> {
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
    fn appservice_in_room(
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
    fn forget(&self, room_id: &RoomId, user_id: &UserId) -> Result<()> {
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
    fn room_servers<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<OwnedServerName>> + 'a> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        Box::new(self.roomserverids.scan_prefix(prefix).map(|(key, _)| {
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
        }))
    }

    #[tracing::instrument(skip(self))]
    fn server_in_room<'a>(&'a self, server: &ServerName, room_id: &RoomId) -> Result<bool> {
        let mut key = server.as_bytes().to_vec();
        key.push(0xff);
        key.extend_from_slice(room_id.as_bytes());

        self.serverroomids.get(&key).map(|o| o.is_some())
    }

    /// Returns an iterator of all rooms a server participates in (as far as we know).
    #[tracing::instrument(skip(self))]
    fn server_rooms<'a>(
        &'a self,
        server: &ServerName,
    ) -> Box<dyn Iterator<Item = Result<OwnedRoomId>> + 'a> {
        let mut prefix = server.as_bytes().to_vec();
        prefix.push(0xff);

        Box::new(self.serverroomids.scan_prefix(prefix).map(|(key, _)| {
            RoomId::parse(
                utils::string_from_bytes(
                    key.rsplit(|&b| b == 0xff)
                        .next()
                        .expect("rsplit always returns an element"),
                )
                .map_err(|_| Error::bad_database("RoomId in serverroomids is invalid unicode."))?,
            )
            .map_err(|_| Error::bad_database("RoomId in serverroomids is invalid."))
        }))
    }

    /// Returns an iterator over all joined members of a room.
    #[tracing::instrument(skip(self))]
    fn room_members<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<OwnedUserId>> + 'a> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        Box::new(self.roomuserid_joined.scan_prefix(prefix).map(|(key, _)| {
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
        }))
    }

    #[tracing::instrument(skip(self))]
    fn room_joined_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
        self.roomid_joinedcount
            .get(room_id.as_bytes())?
            .map(|b| {
                utils::u64_from_bytes(&b)
                    .map_err(|_| Error::bad_database("Invalid joinedcount in db."))
            })
            .transpose()
    }

    #[tracing::instrument(skip(self))]
    fn room_invited_count(&self, room_id: &RoomId) -> Result<Option<u64>> {
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
    fn room_useroncejoined<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<OwnedUserId>> + 'a> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        Box::new(
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
                            Error::bad_database(
                                "User ID in room_useroncejoined is invalid unicode.",
                            )
                        })?,
                    )
                    .map_err(|_| Error::bad_database("User ID in room_useroncejoined is invalid."))
                }),
        )
    }

    /// Returns an iterator over all invited members of a room.
    #[tracing::instrument(skip(self))]
    fn room_members_invited<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<OwnedUserId>> + 'a> {
        let mut prefix = room_id.as_bytes().to_vec();
        prefix.push(0xff);

        Box::new(
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
                }),
        )
    }

    #[tracing::instrument(skip(self))]
    fn get_invite_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
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
    fn get_left_count(&self, room_id: &RoomId, user_id: &UserId) -> Result<Option<u64>> {
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
    fn rooms_joined<'a>(
        &'a self,
        user_id: &UserId,
    ) -> Box<dyn Iterator<Item = Result<OwnedRoomId>> + 'a> {
        Box::new(
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
                }),
        )
    }

    /// Returns an iterator over all rooms a user was invited to.
    #[allow(clippy::type_complexity)]
    #[tracing::instrument(skip(self))]
    fn rooms_invited<'a>(
        &'a self,
        user_id: &UserId,
    ) -> Box<dyn Iterator<Item = Result<(OwnedRoomId, Vec<Raw<AnyStrippedStateEvent>>)>> + 'a> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        Box::new(
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
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_invited is invalid.")
                    })?;

                    let state = serde_json::from_slice(&state).map_err(|_| {
                        Error::bad_database("Invalid state in userroomid_invitestate.")
                    })?;

                    Ok((room_id, state))
                }),
        )
    }

    #[tracing::instrument(skip(self))]
    fn invite_state(
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
    fn left_state(
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
    #[allow(clippy::type_complexity)]
    #[tracing::instrument(skip(self))]
    fn rooms_left<'a>(
        &'a self,
        user_id: &UserId,
    ) -> Box<dyn Iterator<Item = Result<(OwnedRoomId, Vec<Raw<AnySyncStateEvent>>)>> + 'a> {
        let mut prefix = user_id.as_bytes().to_vec();
        prefix.push(0xff);

        Box::new(
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
                    .map_err(|_| {
                        Error::bad_database("Room ID in userroomid_invited is invalid.")
                    })?;

                    let state = serde_json::from_slice(&state).map_err(|_| {
                        Error::bad_database("Invalid state in userroomid_leftstate.")
                    })?;

                    Ok((room_id, state))
                }),
        )
    }

    #[tracing::instrument(skip(self))]
    fn once_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.roomuseroncejoinedids.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    fn is_joined(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_joined.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    fn is_invited(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_invitestate.get(&userroom_id)?.is_some())
    }

    #[tracing::instrument(skip(self))]
    fn is_left(&self, user_id: &UserId, room_id: &RoomId) -> Result<bool> {
        let mut userroom_id = user_id.as_bytes().to_vec();
        userroom_id.push(0xff);
        userroom_id.extend_from_slice(room_id.as_bytes());

        Ok(self.userroomid_leftstate.get(&userroom_id)?.is_some())
    }
}
