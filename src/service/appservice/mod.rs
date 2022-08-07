mod data;
pub use data::Data;

use crate::service::*;

pub struct Service<D: Data> {
    db: D,
}

impl Service<_> {
    /// Registers an appservice and returns the ID to the caller
    pub fn register_appservice(&self, yaml: serde_yaml::Value) -> Result<String> {
        self.db.register_appservice(yaml)
    }

    /// Remove an appservice registration
    ///
    /// # Arguments
    ///
    /// * `service_name` - the name you send to register the service previously
    pub fn unregister_appservice(&self, service_name: &str) -> Result<()> {
        self.db.unregister_appservice(service_name)
    }

    pub fn get_registration(&self, id: &str) -> Result<Option<serde_yaml::Value>> {
        self.db.get_registration(id)
    }

    pub fn iter_ids(&self) -> Result<impl Iterator<Item = Result<String>> + '_> {
        self.db.iter_ids()
    }

    pub fn all(&self) -> Result<Vec<(String, serde_yaml::Value)>> {
        self.db.all()
    }
}
