use std::collections::HashMap;

use conduit_config::Config;

use regex::RegexSet;
use ruma::{
    RoomAliasId, RoomId, UserId,
    api::appservice::{Namespace, Registration},
};
use tokio::{fs::File, io::AsyncReadExt};

use crate::Result;

/// Compiled regular expressions for a namespace.
#[derive(Clone, Debug)]
pub struct NamespaceRegex {
    pub exclusive: Option<RegexSet>,
    pub non_exclusive: Option<RegexSet>,
}

impl NamespaceRegex {
    /// Checks if this namespace has rights to a namespace
    pub fn is_match(&self, heystack: &str) -> bool {
        if self.is_exclusive_match(heystack) {
            return true;
        }

        if let Some(non_exclusive) = &self.non_exclusive
            && non_exclusive.is_match(heystack)
        {
            return true;
        }
        false
    }

    /// Checks if this namespace has exclusive rights to a namespace
    pub fn is_exclusive_match(&self, heystack: &str) -> bool {
        if let Some(exclusive) = &self.exclusive
            && exclusive.is_match(heystack)
        {
            return true;
        }
        false
    }
}

impl TryFrom<Vec<Namespace>> for NamespaceRegex {
    fn try_from(value: Vec<Namespace>) -> Result<Self, regex::Error> {
        let mut exclusive = vec![];
        let mut non_exclusive = vec![];

        for namespace in value {
            if namespace.exclusive {
                exclusive.push(namespace.regex);
            } else {
                non_exclusive.push(namespace.regex);
            }
        }

        Ok(NamespaceRegex {
            exclusive: if exclusive.is_empty() {
                None
            } else {
                Some(RegexSet::new(exclusive)?)
            },
            non_exclusive: if non_exclusive.is_empty() {
                None
            } else {
                Some(RegexSet::new(non_exclusive)?)
            },
        })
    }

    type Error = regex::Error;
}

/// Appservice registration combined with its compiled regular expressions.
#[derive(Clone, Debug)]
pub struct RegistrationInfo {
    pub registration: Registration,
    pub users: NamespaceRegex,
    pub aliases: NamespaceRegex,
    pub rooms: NamespaceRegex,
}

impl RegistrationInfo {
    /// Checks if a given user ID matches either the users namespace or the localpart specified in the appservice registration
    pub fn is_user_match(&self, user_id: &UserId) -> bool {
        self.users.is_match(user_id.as_str())
            || self.registration.sender_localpart == user_id.localpart()
    }

    /// Checks if a given user ID exclusively matches either the users namespace or the localpart specified in the appservice registration
    pub fn is_exclusive_user_match(&self, user_id: &UserId) -> bool {
        self.users.is_exclusive_match(user_id.as_str())
            || self.registration.sender_localpart == user_id.localpart()
    }
}

impl TryFrom<Registration> for RegistrationInfo {
    fn try_from(value: Registration) -> Result<RegistrationInfo, regex::Error> {
        Ok(RegistrationInfo {
            users: value.namespaces.users.clone().try_into()?,
            aliases: value.namespaces.aliases.clone().try_into()?,
            rooms: value.namespaces.rooms.clone().try_into()?,
            registration: value,
        })
    }

    type Error = regex::Error;
}

pub struct Service {
    registration_info: HashMap<String, RegistrationInfo>,
}

impl Service {
    pub async fn build(config: &Config) -> Result<Self> {
        let mut registration_info = HashMap::new();

        for path in config.appservice_registrations.clone() {
            let err = |source| conduit_config::Error::AppserviceRegistrationRead {
                source,
                path: path.clone(),
            };

            let mut file = Vec::new();
            File::open(&path)
                .await
                .map_err(err)?
                .read_to_end(&mut file)
                .await
                .map_err(err)?;

            let registration: Registration = yaml_serde::from_slice(&file).map_err(|source| {
                conduit_config::Error::AppserviceRegistrationDeser { source, path }
            })?;

            registration_info.insert(registration.id.clone(), registration.try_into()?);
        }

        Ok(Self { registration_info })
    }

    pub fn get_registration(&self, id: &str) -> Option<Registration> {
        self.registration_info
            .get(id)
            .cloned()
            .map(|info| info.registration)
    }

    pub fn iter_registrations(&self) -> impl Iterator<Item = &RegistrationInfo> {
        self.registration_info.values()
    }

    pub fn find_from_token(&self, token: &str) -> Option<RegistrationInfo> {
        self.registration_info
            .values()
            .find(|info| info.registration.as_token == token)
            .cloned()
    }

    // Checks if a given user id matches any exclusive appservice regex
    pub fn is_exclusive_user_id(&self, user_id: &UserId) -> bool {
        self.registration_info
            .values()
            .any(|info| info.is_exclusive_user_match(user_id))
    }

    // Checks if a given room alias matches any exclusive appservice regex
    pub fn is_exclusive_alias(&self, alias: &RoomAliasId) -> bool {
        self.registration_info
            .values()
            .any(|info| info.aliases.is_exclusive_match(alias.as_str()))
    }

    // Checks if a given room id matches any exclusive appservice regex
    pub fn is_exclusive_room_id(&self, room_id: &RoomId) -> bool {
        self.registration_info
            .values()
            .any(|info| info.rooms.is_exclusive_match(room_id.as_str()))
    }
}
