pub trait Data {
    type Iter: Iterator;
    /// Registers an appservice and returns the ID to the caller
    fn register_appservice(&self, yaml: serde_yaml::Value) -> Result<String>;

    /// Remove an appservice registration
    ///
    /// # Arguments
    ///
    /// * `service_name` - the name you send to register the service previously
    fn unregister_appservice(&self, service_name: &str) -> Result<()>;

    fn get_registration(&self, id: &str) -> Result<Option<serde_yaml::Value>>;

    fn iter_ids(&self) -> Result<Self::Iter<Item = Result<String>>>;

    fn all(&self) -> Result<Vec<(String, serde_yaml::Value)>>;
}
