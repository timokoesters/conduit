use ruma::api::appservice::Registration;

use crate::Result;

pub trait Data: Send + Sync {
    /// Registers an appservice and returns the ID to the caller
    fn register_appservice(&self, yaml: Registration) -> Result<String>;

    /// Remove an appservice registration
    ///
    /// # Arguments
    ///
    /// * `service_name` - the name you send to register the service previously
    fn unregister_appservice(&self, service_name: &str) -> Result<()>;

    fn get_registration(&self, id: &str) -> Result<Option<Registration>>;

    fn iter_ids<'a>(&'a self) -> Result<Box<dyn Iterator<Item = Result<String>> + 'a>>;

    fn all(&self) -> Result<Vec<(String, Registration)>>;
}
