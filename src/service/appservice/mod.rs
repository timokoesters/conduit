mod data;

use std::collections::HashMap;

pub use data::Data;

use regex::RegexSet;
use ruma::api::appservice::{Namespace, Registration};
use tokio::sync::RwLock;

use crate::{services, Result};

/// Compiled regular expressions for a namespace
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

        if let Some(non_exclusive) = &self.non_exclusive {
            if non_exclusive.is_match(heystack) {
                return true;
            }
        }
        false
    }

    /// Checks if this namespace has exlusive rights to a namespace
    pub fn is_exclusive_match(&self, heystack: &str) -> bool {
        if let Some(exclusive) = &self.exclusive {
            if exclusive.is_match(heystack) {
                return true;
            }
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

/// Compiled regular expressions for an appservice
pub struct RegistrationInfo {
    pub registration: Registration,
    pub users: NamespaceRegex,
    pub aliases: NamespaceRegex,
    pub rooms: NamespaceRegex,
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
    pub db: &'static dyn Data,
    pub registration_info: RwLock<HashMap<String, RegistrationInfo>>,
}

impl Service {
    /// Registers an appservice and returns the ID to the caller
    pub async fn register_appservice(&self, yaml: Registration) -> Result<String> {
        services()
            .appservice
            .registration_info
            .write()
            .await
            .insert(yaml.id.clone(), yaml.clone().try_into()?);

        self.db.register_appservice(yaml)
    }

    /// Remove an appservice registration
    ///
    /// # Arguments
    ///
    /// * `service_name` - the name you send to register the service previously
    pub async fn unregister_appservice(&self, service_name: &str) -> Result<()> {
        services()
            .appservice
            .registration_info
            .write()
            .await
            .remove(service_name);

        self.db.unregister_appservice(service_name)
    }

    pub fn get_registration(&self, id: &str) -> Result<Option<Registration>> {
        self.db.get_registration(id)
    }

    pub fn iter_ids(&self) -> Result<impl Iterator<Item = Result<String>> + '_> {
        self.db.iter_ids()
    }

    pub fn all(&self) -> Result<Vec<(String, Registration)>> {
        self.db.all()
    }
}
