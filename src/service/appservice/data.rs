pub trait Data {
    /// Registers an appservice and returns the ID to the caller
    pub fn register_appservice(&self, yaml: serde_yaml::Value) -> Result<String>;

    /// Remove an appservice registration
    ///
    /// # Arguments
    ///
    /// * `service_name` - the name you send to register the service previously
    pub fn unregister_appservice(&self, service_name: &str) -> Result<()>;

    pub fn get_registration(&self, id: &str) -> Result<Option<serde_yaml::Value>>;

    pub fn iter_ids(&self) -> Result<impl Iterator<Item = Result<String>> + '_>;

    pub fn all(&self) -> Result<Vec<(String, serde_yaml::Value)>>;
}
