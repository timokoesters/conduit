use crate::{utils, Error, Result};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Clone)]
pub struct Appservice {
    pub(super) cached_registrations: Arc<RwLock<HashMap<String, serde_yaml::Value>>>,
    pub(super) id_appserviceregistrations: sled::Tree,
}

impl Appservice {
    pub fn register_appservice(&self, yaml: serde_yaml::Value) -> Result<()> {
        // TODO: Rumaify
        let id = yaml.get("id").unwrap().as_str().unwrap();
        self.id_appserviceregistrations
            .insert(id, serde_yaml::to_string(&yaml).unwrap().as_bytes())?;
        self.cached_registrations
            .write()
            .unwrap()
            .insert(id.to_owned(), yaml);

        Ok(())
    }

    pub fn get_registration(&self, id: &str) -> Result<Option<serde_yaml::Value>> {
        self.cached_registrations
            .read()
            .unwrap()
            .get(id)
            .map_or_else(
                || {
                    Ok(self
                        .id_appserviceregistrations
                        .get(id)?
                        .map(|bytes| {
                            Ok::<_, Error>(serde_yaml::from_slice(&bytes).map_err(|_| {
                                Error::bad_database(
                                    "Invalid registration bytes in id_appserviceregistrations.",
                                )
                            })?)
                        })
                        .transpose()?)
                },
                |r| Ok(Some(r.clone())),
            )
    }

    pub fn iter_ids(&self) -> impl Iterator<Item = Result<String>> {
        self.id_appserviceregistrations.iter().keys().map(|id| {
            Ok(utils::string_from_bytes(&id?).map_err(|_| {
                Error::bad_database("Invalid id bytes in id_appserviceregistrations.")
            })?)
        })
    }

    pub fn iter_all<'a>(&'a self) -> impl Iterator<Item = Result<(String, serde_yaml::Value)>> + 'a {
        self.iter_ids().filter_map(|id| id.ok()).map(move |id| {
            Ok((id.clone(), self.get_registration(&id)?.expect("iter_ids only returns appservices that exist")))
        })
    }
}
