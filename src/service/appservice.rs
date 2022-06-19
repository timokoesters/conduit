use crate::{utils, Error, Result};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use super::abstraction::Tree;

pub struct Appservice {
    pub(super) cached_registrations: Arc<RwLock<HashMap<String, serde_yaml::Value>>>,
    pub(super) id_appserviceregistrations: Arc<dyn Tree>,
}

impl Appservice {
    /// Registers an appservice and returns the ID to the caller
    ///
    pub fn register_appservice(&self, yaml: serde_yaml::Value) -> Result<String> {
        // TODO: Rumaify
        let id = yaml.get("id").unwrap().as_str().unwrap();
        self.id_appserviceregistrations.insert(
            id.as_bytes(),
            serde_yaml::to_string(&yaml).unwrap().as_bytes(),
        )?;
        self.cached_registrations
            .write()
            .unwrap()
            .insert(id.to_owned(), yaml.to_owned());

        Ok(id.to_owned())
    }

    /// Remove an appservice registration
    ///
    /// # Arguments
    ///
    /// * `service_name` - the name you send to register the service previously
    pub fn unregister_appservice(&self, service_name: &str) -> Result<()> {
        self.id_appserviceregistrations
            .remove(service_name.as_bytes())?;
        self.cached_registrations
            .write()
            .unwrap()
            .remove(service_name);
        Ok(())
    }

    pub fn get_registration(&self, id: &str) -> Result<Option<serde_yaml::Value>> {
        self.cached_registrations
            .read()
            .unwrap()
            .get(id)
            .map_or_else(
                || {
                    self.id_appserviceregistrations
                        .get(id.as_bytes())?
                        .map(|bytes| {
                            serde_yaml::from_slice(&bytes).map_err(|_| {
                                Error::bad_database(
                                    "Invalid registration bytes in id_appserviceregistrations.",
                                )
                            })
                        })
                        .transpose()
                },
                |r| Ok(Some(r.clone())),
            )
    }

    pub fn iter_ids(&self) -> Result<impl Iterator<Item = Result<String>> + '_> {
        Ok(self.id_appserviceregistrations.iter().map(|(id, _)| {
            utils::string_from_bytes(&id)
                .map_err(|_| Error::bad_database("Invalid id bytes in id_appserviceregistrations."))
        }))
    }

    pub fn all(&self) -> Result<Vec<(String, serde_yaml::Value)>> {
        self.iter_ids()?
            .filter_map(|id| id.ok())
            .map(move |id| {
                Ok((
                    id.clone(),
                    self.get_registration(&id)?
                        .expect("iter_ids only returns appservices that exist"),
                ))
            })
            .collect()
    }
}
