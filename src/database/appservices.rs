use crate::Error;
use http::Uri;
use regex::Regex;
use ruma::identifiers::{RoomAliasId, RoomId, UserId};
use serde::Deserialize;
use std::{collections::HashMap, convert::TryFrom, fs::File, io::BufReader, path::Path};

/// A map from AppService token to appservice registration.
pub struct AppServices(HashMap<String, AppService>);

#[derive(Debug, Deserialize)]
pub struct AppService {
    /// Required. A unique, user-defined ID of the application service which will never change.
    id: String,
    /// Required. The URL for the application service. May include appsrv path after the domain name. Optionally set to null if no traffic is required.
    #[serde(default)]
    #[serde(deserialize_with = "deserialize_url")]
    url: Option<Uri>,
    /// Required. A unique token for application services to use to authenticate requests to Homeservers.
    // We will be comparing this token to untrusted user input!
    // Thus it is important that we perform constant-time comparisons, to avoid leaking the token in appsrv timing attack.
    as_token: String,
    /// Required. A unique token for Homeservers to use to authenticate requests to application services.
    hs_token: String,
    /// Required. The localpart of the user associated with the application service.
    #[serde(deserialize_with = "deserialize_userid_without_servername")]
    #[serde(rename = "sender_localpart")]
    pub service_user: UserId,
    /// Required. A list of users, aliases and rooms namespaces that the application service controls.
    namespaces: Namespaces,
    /// The external protocols which the application service provides (e.g. IRC).
    #[serde(default)]
    protocols: Vec<String>,
    /// Whether requests from masqueraded users are rate-limited. The sender is excluded.
    #[serde(default)]
    rate_limited: bool,
}

#[derive(Debug, Deserialize)]
struct Namespaces {
    /// Events which are sent from certain users.
    #[serde(default)]
    users: Vec<Namespace>,
    /// Events which are sent in rooms with certain room aliases.
    #[serde(default)]
    aliases: Vec<Namespace>,
    /// Events which are sent in rooms with certain room IDs.
    #[serde(default)]
    rooms: Vec<Namespace>,
}

#[derive(Debug, Deserialize)]
struct Namespace {
    /// Required. A true or false value stating whether this application service has exclusive access to events within this namespace.
    exclusive: bool,
    /// Required. A regular expression defining which values this namespace includes.
    #[serde(deserialize_with = "deserialize_regex")]
    regex: Regex,
}

#[derive(Debug, PartialOrd, Ord, PartialEq, Eq)]
pub enum InterestLevel {
    Uninterested,
    Shared,
    Exclusive,
}

impl Namespace {
    /// Checks how much permission an appservice has over the namespace.
    fn check_interest(&self, id: impl AsRef<str>) -> InterestLevel {
        use InterestLevel::*;
        match (self.regex.is_match(id.as_ref()), self.exclusive) {
            (true, true) => Exclusive,
            (true, false) => Shared,
            (false, _) => Uninterested,
        }
    }
}

impl AppService {
    fn max_interest_level(namespaces: &[Namespace], id: impl AsRef<str>) -> InterestLevel {
        let id_str = id.as_ref();
        namespaces
            .iter()
            .map(|ns| ns.check_interest(id_str))
            .max()
            .unwrap_or(InterestLevel::Uninterested)
    }

    pub fn interest_in_user(&self, user: &UserId) -> InterestLevel {
        Self::max_interest_level(&self.namespaces.users, user)
    }

    pub fn interest_in_alias(&self, alias: &RoomAliasId) -> InterestLevel {
        Self::max_interest_level(&self.namespaces.aliases, alias)
    }

    pub fn interest_in_room(&self, room: &RoomId) -> InterestLevel {
        Self::max_interest_level(&self.namespaces.rooms, room)
    }
}

impl AppServices {
    pub fn new(
        paths: impl IntoIterator<Item = impl AsRef<Path>>,
        server_name: &str,
    ) -> Result<AppServices, Error> {
        //  Each as_token and id MUST be unique per application service
        //  (https://matrix.org/docs/spec/application_service/r0.1.2#registration)

        let mut services = AppServices(HashMap::new());

        for path in paths {
            let service_reader = BufReader::new(
                File::open(path)
                    .map_err(|_| Error::BadConfig("Failed to open a registration file."))?,
            );

            let mut service: AppService = serde_yaml::from_reader(service_reader)
                .map_err(|_| Error::BadConfig("Failed to parse registration file as YAML."))?;

            service.service_user = UserId::try_from(format!(
                "@{}:{}",
                service.service_user.localpart(),
                server_name
            ))
            .map_err(|_| Error::BadConfig("Invalid sender_localpart in registration."))?;

            if services.from_token(&service.as_token).is_some() {
                return Err(Error::BadConfig(
                    "Multiple appservices are registered with the same as_token.",
                ));
            }

            if services.find_id(&service.id).is_some() {
                return Err(Error::BadConfig(
                    "Multiple appservices are registered with the same id.",
                ));
            }

            services.0.insert(service.as_token.clone(), service);
        }

        Ok(services)
    }

    pub fn from_token(&self, token: &str) -> Option<&AppService> {
        self.0.get(token)
    }

    pub fn find_id(&self, id: &str) -> Option<&AppService> {
        self.0.values().find(|appsrv| appsrv.id == id)
    }
}

fn deserialize_url<'de, D>(d: D) -> Result<Option<Uri>, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    Ok(Some(
        String::deserialize(d)?
            .parse()
            .map_err(serde::de::Error::custom)?,
    ))
}

fn deserialize_regex<'de, D>(d: D) -> Result<Regex, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let mut re_str = String::deserialize(d)?;
    re_str.insert(0, '^');
    re_str.push('$');
    Regex::new(&re_str).map_err(serde::de::Error::custom)
}

fn deserialize_userid_without_servername<'de, D>(d: D) -> Result<UserId, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let mut userid = String::deserialize(d)?;
    userid.insert(0, '@');
    // At deserialization time, we do not have access to the real configured server name
    userid.push_str(":temp-server-name");
    UserId::try_from(userid.as_str()).map_err(serde::de::Error::custom)
}
