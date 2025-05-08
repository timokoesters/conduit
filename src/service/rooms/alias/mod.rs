mod data;

pub use data::Data;
use rand::seq::SliceRandom;
use tracing::error;

use crate::{services, Error, Result};
use ruma::{
    api::{
        appservice,
        client::{alias::get_alias, error::ErrorKind},
        federation,
    },
    events::{
        room::power_levels::{RoomPowerLevels, RoomPowerLevelsEventContent},
        StateEventType,
    },
    OwnedRoomAliasId, OwnedRoomId, RoomAliasId, RoomId, UserId,
};

pub struct Service {
    pub db: &'static dyn Data,
}

impl Service {
    #[tracing::instrument(skip(self))]
    pub fn set_alias(&self, alias: &RoomAliasId, room_id: &RoomId, user_id: &UserId) -> Result<()> {
        if alias == services().globals.admin_alias() && user_id != services().globals.server_user()
        {
            Err(Error::BadRequest(
                ErrorKind::forbidden(),
                "Only the server user can set this alias",
            ))
        } else {
            self.db.set_alias(alias, room_id, user_id)
        }
    }

    #[tracing::instrument(skip(self))]
    fn user_can_remove_alias(&self, alias: &RoomAliasId, user_id: &UserId) -> Result<bool> {
        let Some(room_id) = self.resolve_local_alias(alias)? else {
            return Err(Error::BadRequest(ErrorKind::NotFound, "Alias not found."));
        };

        // The creator of an alias can remove it
        if self
            .db
            .who_created_alias(alias)?
            .map(|user| user == user_id)
            .unwrap_or_default()
            // Server admins can remove any local alias
            || services().admin.user_is_admin(user_id)?
            // Always allow the Conduit user to remove the alias, since there may not be an admin room
            || services().globals.server_user ()== user_id
        {
            Ok(true)
            // Checking whether the user is able to change canonical aliases of the room
        } else if let Some(event) = services().rooms.state_accessor.room_state_get(
            &room_id,
            &StateEventType::RoomPowerLevels,
            "",
        )? {
            serde_json::from_str(event.content.get())
                .map_err(|_| Error::bad_database("Invalid event content for m.room.power_levels"))
                .map(|content: RoomPowerLevelsEventContent| {
                    RoomPowerLevels::from(content)
                        .user_can_send_state(user_id, StateEventType::RoomCanonicalAlias)
                })
        // If there is no power levels event, only the room creator can change canonical aliases
        } else if let Some(event) = services().rooms.state_accessor.room_state_get(
            &room_id,
            &StateEventType::RoomCreate,
            "",
        )? {
            Ok(event.sender == user_id)
        } else {
            error!("Room {} has no m.room.create event (VERY BAD)!", room_id);
            Err(Error::bad_database("Room has no m.room.create event"))
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn remove_alias(&self, alias: &RoomAliasId, user_id: &UserId) -> Result<()> {
        if self.user_can_remove_alias(alias, user_id)? {
            self.db.remove_alias(alias)
        } else {
            Err(Error::BadRequest(
                ErrorKind::forbidden(),
                "User is not permitted to remove this alias.",
            ))
        }
    }

    #[tracing::instrument(skip(self))]
    pub fn resolve_local_alias(&self, alias: &RoomAliasId) -> Result<Option<OwnedRoomId>> {
        self.db.resolve_local_alias(alias)
    }

    #[tracing::instrument(skip(self))]
    pub fn local_aliases_for_room<'a>(
        &'a self,
        room_id: &RoomId,
    ) -> Box<dyn Iterator<Item = Result<OwnedRoomAliasId>> + 'a> {
        self.db.local_aliases_for_room(room_id)
    }

    /// Resolves an alias to a room id, and a set of servers to join or knock via, either locally or over federation
    #[tracing::instrument(skip(self))]
    pub async fn get_alias_helper(
        &self,
        room_alias: OwnedRoomAliasId,
    ) -> Result<get_alias::v3::Response> {
        if room_alias.server_name() != services().globals.server_name() {
            let response = services()
                .sending
                .send_federation_request(
                    room_alias.server_name(),
                    federation::query::get_room_information::v1::Request {
                        room_alias: room_alias.to_owned(),
                    },
                )
                .await?;

            let mut servers = response.servers;
            servers.shuffle(&mut rand::rng());

            return Ok(get_alias::v3::Response::new(response.room_id, servers));
        }

        let mut room_id = None;
        match services().rooms.alias.resolve_local_alias(&room_alias)? {
            Some(r) => room_id = Some(r),
            None => {
                for appservice in services().appservice.read().await.values() {
                    if appservice.aliases.is_match(room_alias.as_str())
                        && matches!(
                            services()
                                .sending
                                .send_appservice_request(
                                    appservice.registration.clone(),
                                    appservice::query::query_room_alias::v1::Request {
                                        room_alias: room_alias.clone(),
                                    },
                                )
                                .await,
                            Ok(Some(_opt_result))
                        )
                    {
                        room_id =
                            Some(self.resolve_local_alias(&room_alias)?.ok_or_else(|| {
                                Error::bad_config("Appservice lied to us. Room does not exist.")
                            })?);
                        break;
                    }
                }
            }
        };

        let room_id = match room_id {
            Some(room_id) => room_id,
            None => {
                return Err(Error::BadRequest(
                    ErrorKind::NotFound,
                    "Room with alias not found.",
                ))
            }
        };

        Ok(get_alias::v3::Response::new(
            room_id,
            vec![services().globals.server_name().to_owned()],
        ))
    }
}
