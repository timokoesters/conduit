use std::{collections::HashMap, num::NonZeroU64};

use bytesize::ByteSize;
use ruma::api::Metadata;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct WrappedShadowConfig {
    #[serde(default)]
    pub inherits: ConfigPreset,
    #[serde(flatten)]
    pub config: ShadowConfig,
}

impl From<WrappedShadowConfig> for Config {
    fn from(value: WrappedShadowConfig) -> Self {
        Config::get_preset(value.inherits).apply_overrides(value.config)
    }
}

#[derive(Debug, Clone, Deserialize, Default, Copy)]
#[cfg_attr(
    feature = "doc-generators",
    derive(conduit_macros::DocumentEnum, serde::Serialize)
)]
#[serde(rename_all = "snake_case")]
pub enum ConfigPreset {
    /// The default preset, designed for small private servers (i.e. single-user or for family and
    /// friends).
    #[default]
    PrivateSmall,
    /// Designed for medium-sized private servers (e.g. for an entire school class or year-group).
    PrivateMedium,
    /// For medium-sized public servers (i.e. you intend 20-100 users to actively use it).
    PublicMedium,
    /// For larger public server (i.e. you intend 200-1000 users to actively use it).
    PublicLarge,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShadowConfig {
    pub client:
        ShadowConfigFragment<ClientRestriction, ShadowClientMediaConfig, AuthenticationFailures>,
    pub federation:
        ShadowConfigFragment<FederationRestriction, ShadowFederationMediaConfig, Nothing>,
}

pub trait RestrictionGeneric: ConfigPart + std::hash::Hash + Eq {}
impl<T> RestrictionGeneric for T where T: ConfigPart + std::hash::Hash + Eq {}

pub trait ConfigPart: Clone + std::fmt::Debug + serde::de::DeserializeOwned {}
impl<T> ConfigPart for T where T: Clone + std::fmt::Debug + serde::de::DeserializeOwned {}

#[derive(Debug, Clone, Deserialize)]
pub struct ShadowConfigFragment<R, M, T>
where
    R: RestrictionGeneric,
    M: ConfigPart,
    T: ConfigPart,
{
    #[serde(bound(deserialize = "R: RestrictionGeneric, M: ConfigPart, T: ConfigPart"))]
    pub target: Option<ShadowConfigFragmentFragment<R, M, T>>,
    #[serde(bound(deserialize = "R: RestrictionGeneric, M: ConfigPart"))]
    pub global: Option<ShadowConfigFragmentFragment<R, M, Nothing>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ShadowConfigFragmentFragment<R, M, T>
where
    R: RestrictionGeneric,
    M: ConfigPart,
    T: ConfigPart,
{
    #[serde(
        flatten,
        // https://play.rust-lang.org/?version=stable&mode=debug&edition=2024&gist=fe75063b73c6d9860991c41572e00035
        // 
        // For some reason specifying the default function fixes the issue in the playground link
        // above. Makes no sense to me, but hey, it works.
        default = "HashMap::new",
        bound(deserialize = "R: RestrictionGeneric")
    )]
    pub map: HashMap<R, RequestLimitation>,
    #[serde(bound(deserialize = "M: ConfigPart"))]
    pub media: Option<M>,
    #[serde(flatten)]
    #[serde(bound(deserialize = "T: ConfigPart"))]
    pub additional_fields: Option<T>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ShadowClientMediaConfig {
    pub download: Option<MediaLimitation>,
    pub upload: Option<MediaLimitation>,
    pub fetch: Option<MediaLimitation>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ShadowFederationMediaConfig {
    pub download: Option<MediaLimitation>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(from = "WrappedShadowConfig")]
pub struct Config {
    pub target: ConfigFragment<AuthenticationFailures>,
    pub global: ConfigFragment<Nothing>,
}

impl Default for Config {
    fn default() -> Self {
        Self::get_preset(ConfigPreset::default())
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigFragment<T>
where
    T: ConfigPart,
{
    #[serde(bound(deserialize = "T: ConfigPart"))]
    pub client: ConfigFragmentFragment<ClientRestriction, ClientMediaConfig, T>,
    pub federation: ConfigFragmentFragment<FederationRestriction, FederationMediaConfig, Nothing>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigFragmentFragment<R, M, T>
where
    R: RestrictionGeneric,
    M: ConfigPart,
    T: ConfigPart,
{
    #[serde(flatten)]
    #[serde(bound(deserialize = "R: RestrictionGeneric"))]
    pub map: HashMap<R, RequestLimitation>,
    #[serde(bound(deserialize = "M: ConfigPart"))]
    pub media: M,
    #[serde(flatten)]
    #[serde(bound(deserialize = "T: ConfigPart"))]
    pub additional_fields: T,
}

impl<R, M, T> ConfigFragmentFragment<R, M, T>
where
    R: RestrictionGeneric,
    M: ConfigPart + MediaConfig,
    T: ConfigPart,
{
    pub fn apply_overrides(
        self,
        shadow: Option<ShadowConfigFragmentFragment<R, M::Shadow, T>>,
    ) -> Self {
        let Some(shadow) = shadow else {
            return self;
        };

        let ConfigFragmentFragment {
            mut map,
            media,
            additional_fields,
        } = self;

        map.extend(shadow.map);

        Self {
            map,
            media: if let Some(sm) = shadow.media {
                media.apply_overrides(sm)
            } else {
                media
            },
            additional_fields: shadow.additional_fields.unwrap_or(additional_fields),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[cfg_attr(
    feature = "doc-generators",
    derive(conduit_macros::DocumentStruct, conduit_macros::GetStructFieldValue)
)]
pub struct AuthenticationFailures {
    /// This request restriction is a bit different from the previous ones.
    /// This one only starts to have it's capacity used if authenticated requests are made with an
    /// invalid authentictation token.
    ///
    /// This may happen non-maliciously, e.g. when a device is
    /// logged out. But a bad actor can keep making requests with random values as the
    /// authentication tokens until one of them are found to be valid. This restriction prevents
    /// that by preventing all authenticated (client) requests while this limit is exceeded.
    ///
    /// Because of this, this restriction cannot be applied globally, only per-target, because an
    /// exceeded global limit would prevent all global requests, which can be exploited.
    pub authentication_failures: RequestLimitation,
}

impl AuthenticationFailures {
    fn new(timeframe: Timeframe, burst_capacity: NonZeroU64) -> Self {
        Self {
            authentication_failures: RequestLimitation::new(timeframe, burst_capacity),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Nothing;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Restriction {
    Client(ClientRestriction),
    Federation(FederationRestriction),
}

impl From<ClientRestriction> for Restriction {
    fn from(value: ClientRestriction) -> Self {
        Self::Client(value)
    }
}

impl From<FederationRestriction> for Restriction {
    fn from(value: FederationRestriction) -> Self {
        Self::Federation(value)
    }
}

#[cfg(feature = "doc-generators")]
pub trait DocumentEnum: Sized {
    fn variant_doc_comments() -> Vec<(Self, String)>;
    fn container_doc_comment() -> String;
}

#[cfg(feature = "doc-generators")]
pub trait DocumentStruct: Sized {
    fn field_doc_comments() -> Vec<(String, String)>;
    fn container_doc_comment() -> String;
}

/// Applies for endpoints on the client-server API, which are used by clients, appservices, and
/// bots. Appservices can bypass rate-limiting though if `rate_limited` is set to `false` in their
/// registration file.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(
    feature = "doc-generators",
    derive(conduit_macros::DocumentEnum, serde::Serialize)
)]
#[serde(rename_all = "snake_case")]
pub enum ClientRestriction {
    /// For registering a new user account. May be called multiples times for a single
    /// registration if there are extra steps, e.g. providing a registration token.
    Registration,
    /// For logging into an existing account.
    Login,
    /// For checking whether a given registration token would allow the user to register an
    /// account.
    RegistrationTokenValidity,

    /// For sending an event to a room.
    ///
    /// Note that this is not used for state events, but for users who are unprivliged in a room,
    /// the only state event they'll be able to send are ones to update their room profile.
    SendEvent,

    /// For joining a room.
    Join,
    /// For inviting a user to a room.
    Invite,
    /// For knocking on a room.
    Knock,

    /// For reporting a user, event, or room.
    SendReport,

    /// For adding an alias to a room.
    CreateAlias,

    /// For downloading a media file.
    ///
    /// For rate-limiting based on the size of files downloaded, see the media rate-limiting
    /// configuration.
    MediaDownload,
    /// For uploading a media file.
    ///
    /// For rate-limiting based on the size of files uploaded, see the media rate-limiting
    /// configuration.
    MediaCreate,
}

/// Applies for endpoints on the federation API of this server, hence restricting how
/// many times other servers can use these endpoints on this server in a given timeframe.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[cfg_attr(
    feature = "doc-generators",
    derive(conduit_macros::DocumentEnum, serde::Serialize)
)]
#[serde(rename_all = "snake_case")]
pub enum FederationRestriction {
    /// For joining a room.
    Join,
    /// For knocking on a room.
    Knock,
    /// For inviting a local user to a room.
    Invite,

    // Transactions should be handled by a completely dedicated rate-limiter
    /* /// For sending transactions of PDU/EDUs.
    ///
    ///
    Transaction, */
    /// For downloading media.
    MediaDownload,
}

impl TryFrom<Metadata> for Restriction {
    type Error = ();

    fn try_from(value: Metadata) -> Result<Self, Self::Error> {
        use Restriction::*;
        use ruma::api::{
            IncomingRequest,
            client::{
                account::{check_registration_token_validity, register},
                alias::create_alias,
                authenticated_media::{
                    get_content, get_content_as_filename, get_content_thumbnail, get_media_preview,
                },
                knock::knock_room,
                media::{self, create_content, create_mxc_uri},
                membership::{invite_user, join_room_by_id, join_room_by_id_or_alias},
                message::send_message_event,
                reporting::report_user,
                room::{report_content, report_room},
                session::login,
                state::send_state_event,
            },
            federation::{
                authenticated_media::{
                    get_content as federation_get_content,
                    get_content_thumbnail as federation_get_content_thumbnail,
                },
                membership::{create_invite, create_join_event, create_knock_event},
            },
        };

        Ok(match value {
            register::v3::Request::METADATA => Client(ClientRestriction::Registration),
            check_registration_token_validity::v1::Request::METADATA => {
                Client(ClientRestriction::RegistrationTokenValidity)
            }
            login::v3::Request::METADATA => Client(ClientRestriction::Login),
            send_message_event::v3::Request::METADATA | send_state_event::v3::Request::METADATA => {
                Client(ClientRestriction::SendEvent)
            }
            join_room_by_id::v3::Request::METADATA
            | join_room_by_id_or_alias::v3::Request::METADATA => Client(ClientRestriction::Join),
            invite_user::v3::Request::METADATA => Client(ClientRestriction::Invite),
            knock_room::v3::Request::METADATA => Client(ClientRestriction::Knock),
            report_user::v3::Request::METADATA
            | report_content::v3::Request::METADATA
            | report_room::v3::Request::METADATA => Client(ClientRestriction::SendReport),
            create_alias::v3::Request::METADATA => Client(ClientRestriction::CreateAlias),
            // NOTE: handle async media upload in a way that doesn't half the number of uploads you can do within a short timeframe, while not allowing pre-generation of MXC uris to allow uploading double the number of media at once
            create_content::v3::Request::METADATA | create_mxc_uri::v1::Request::METADATA => {
                Client(ClientRestriction::MediaCreate)
            }
            // Unauthenticate media is deprecated
            #[allow(deprecated)]
            media::get_content::v3::Request::METADATA
            | media::get_content_as_filename::v3::Request::METADATA
            | media::get_content_thumbnail::v3::Request::METADATA
            | media::get_media_preview::v3::Request::METADATA
            | get_content::v1::Request::METADATA
            | get_content_as_filename::v1::Request::METADATA
            | get_content_thumbnail::v1::Request::METADATA
            | get_media_preview::v1::Request::METADATA => Client(ClientRestriction::MediaDownload),
            federation_get_content::v1::Request::METADATA
            | federation_get_content_thumbnail::v1::Request::METADATA => {
                Federation(FederationRestriction::MediaDownload)
            }
            // v1 is deprecated
            #[allow(deprecated)]
            create_join_event::v1::Request::METADATA | create_join_event::v2::Request::METADATA => {
                Federation(FederationRestriction::Join)
            }
            create_knock_event::v1::Request::METADATA => Federation(FederationRestriction::Knock),
            create_invite::v1::Request::METADATA | create_invite::v2::Request::METADATA => {
                Federation(FederationRestriction::Invite)
            }

            _ => return Err(()),
        })
    }
}

impl<T> ConfigFragment<T>
where
    T: ConfigPart,
{
    pub fn get(&self, restriction: &Restriction) -> &RequestLimitation {
        // Maybe look into https://github.com/moriyoshi-kasuga/enum-table
        match restriction {
            Restriction::Client(client_restriction) => self
                .client
                .map
                .get(client_restriction)
                .expect("For every restriction, we have a preset limitation configuration"),
            Restriction::Federation(federation_restriction) => self
                .federation
                .map
                .get(federation_restriction)
                .expect("For every restriction, we have a preset limitation configuration"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct RequestLimitation {
    #[serde(flatten)]
    pub timeframe: Timeframe,
    pub burst_capacity: NonZeroU64,
}

impl RequestLimitation {
    pub fn new(timeframe: Timeframe, burst_capacity: NonZeroU64) -> Self {
        Self {
            timeframe,
            burst_capacity,
        }
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
// When deserializing, we want this prefix
#[allow(clippy::enum_variant_names)]
pub enum Timeframe {
    PerSecond(NonZeroU64),
    PerMinute(NonZeroU64),
    PerHour(NonZeroU64),
    PerDay(NonZeroU64),
}

#[cfg(feature = "doc-generators")]
impl std::fmt::Display for Timeframe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value;

        let string = match self {
            Self::PerSecond(v) => {
                value = v;
                "second"
            }
            Self::PerMinute(v) => {
                value = v;
                "minute"
            }
            Self::PerHour(v) => {
                value = v;
                "hour"
            }
            Self::PerDay(v) => {
                value = v;
                "day"
            }
        };
        write!(f, "{value} requests per {string}")
    }
}

impl Timeframe {
    pub fn nano_gap(&self) -> u64 {
        match self {
            Timeframe::PerSecond(t) => 1000 * 1000 * 1000 / t.get(),
            Timeframe::PerMinute(t) => 1000 * 1000 * 1000 * 60 / t.get(),
            Timeframe::PerHour(t) => 1000 * 1000 * 1000 * 60 * 60 / t.get(),
            Timeframe::PerDay(t) => 1000 * 1000 * 1000 * 60 * 60 * 24 / t.get(),
        }
    }
}

pub trait MediaConfig {
    type Shadow: ConfigPart;

    fn apply_overrides(self, shadow: Self::Shadow) -> Self;
}

#[cfg(feature = "doc-generators")]
pub trait GetStructFieldValue: Sized {
    type Limitation;
    fn get(&self, field: &str) -> Option<Self::Limitation>;
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[cfg_attr(
    feature = "doc-generators",
    derive(conduit_macros::DocumentStruct, conduit_macros::GetStructFieldValue)
)]
pub struct ClientMediaConfig {
    /// This restriction is applied whenever a client downloads media from the server.
    pub download: MediaLimitation,
    /// This restriction is applied whenever a client uploads media to the server.
    pub upload: MediaLimitation,
    /// In addition to the `download`, the `fetch` restriction is also applied when the media that
    /// a client has downloaded was had to be fetched from a remote server, due to it not already
    /// being stored.
    pub fetch: MediaLimitation,
}

impl MediaConfig for ClientMediaConfig {
    type Shadow = ShadowClientMediaConfig;

    fn apply_overrides(self, shadow: Self::Shadow) -> Self {
        let Self::Shadow {
            download,
            upload,
            fetch,
        } = shadow;

        Self {
            download: download.unwrap_or(self.download),
            upload: upload.unwrap_or(self.upload),
            fetch: fetch.unwrap_or(self.fetch),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[cfg_attr(
    feature = "doc-generators",
    derive(conduit_macros::DocumentStruct, conduit_macros::GetStructFieldValue)
)]
pub struct FederationMediaConfig {
    /// This restriction is applied whenever a remote server downloads media from this server.
    pub download: MediaLimitation,
}

impl MediaConfig for FederationMediaConfig {
    type Shadow = ShadowFederationMediaConfig;

    fn apply_overrides(self, shadow: Self::Shadow) -> Self {
        Self {
            download: shadow.download.unwrap_or(self.download),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize)]
pub struct MediaLimitation {
    #[serde(flatten)]
    pub timeframe: MediaTimeframe,
    pub burst_capacity: ByteSize,
}

impl MediaLimitation {
    pub fn new(timeframe: MediaTimeframe, burst_capacity: ByteSize) -> Self {
        Self {
            timeframe,
            burst_capacity,
        }
    }
}

#[derive(Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "snake_case")]
// When deserializing, we want this prefix
#[allow(clippy::enum_variant_names)]
pub enum MediaTimeframe {
    PerSecond(ByteSize),
    PerMinute(ByteSize),
    PerHour(ByteSize),
    PerDay(ByteSize),
}

#[cfg(feature = "doc-generators")]
impl std::fmt::Display for MediaTimeframe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value;

        let string = match self {
            Self::PerSecond(v) => {
                value = v;
                "second"
            }
            Self::PerMinute(v) => {
                value = v;
                "minute"
            }
            Self::PerHour(v) => {
                value = v;
                "hour"
            }
            Self::PerDay(v) => {
                value = v;
                "day"
            }
        };
        write!(f, "{} per {string}", value.display().si())
    }
}
impl MediaTimeframe {
    pub fn bytes_per_sec(&self) -> u64 {
        match self {
            MediaTimeframe::PerSecond(t) => t.as_u64(),
            MediaTimeframe::PerMinute(t) => t.as_u64() / 60,
            MediaTimeframe::PerHour(t) => t.as_u64() / (60 * 60),
            MediaTimeframe::PerDay(t) => t.as_u64() / (60 * 60 * 24),
        }
    }
}

fn nz(int: u64) -> NonZeroU64 {
    NonZeroU64::new(int).expect("Values are static")
}

macro_rules! default_restriction_map {
    ($restriction_type:ident; $($restriction:ident, $timeframe:ident, $timeframe_value:expr, $burst_capacity:expr;)*) => {
            HashMap::from_iter([
                $((
                    $restriction_type::$restriction,
                    RequestLimitation::new(Timeframe::$timeframe(nz($timeframe_value)), nz($burst_capacity)),
                ),)*
            ])
    }
}

macro_rules! media_config {
    ($config_type:ident; $($key:ident: $timeframe:ident, $timeframe_value:expr, $burst_capacity:expr;)*) => {
        $config_type {
            $($key: MediaLimitation::new(MediaTimeframe::$timeframe($timeframe_value), $burst_capacity),)*
        }
    }
}

impl Config {
    fn apply_overrides(self, shadow: ShadowConfig) -> Self {
        let ShadowConfig {
            client:
                ShadowConfigFragment {
                    target: client_target,
                    global: client_global,
                },
            federation:
                ShadowConfigFragment {
                    target: federation_target,
                    global: federation_global,
                },
        } = shadow;

        Self {
            target: ConfigFragment {
                client: self.target.client.apply_overrides(client_target),
                federation: self.target.federation.apply_overrides(federation_target),
            },
            global: ConfigFragment {
                client: self.global.client.apply_overrides(client_global),
                federation: self.global.federation.apply_overrides(federation_global),
            },
        }
    }

    pub fn get_preset(preset: ConfigPreset) -> Self {
        // The client target map shouldn't really differ between presets, as individual user's
        // behaviours shouldn't differ depending on the size of the server or whether it's private
        // or public, but maybe I'm wrong.
        let target_client_map = default_restriction_map!(
            ClientRestriction;

            Registration, PerDay, 3, 10;
            Login, PerDay, 5, 20;
            RegistrationTokenValidity, PerDay, 10, 20;
            SendEvent, PerMinute, 15, 60;
            Join, PerHour, 5, 30;
            Knock, PerHour, 5, 30;
            Invite, PerHour, 2, 20;
            SendReport, PerDay, 5, 20;
            CreateAlias, PerDay, 2, 20;
            MediaDownload, PerHour, 30, 100;
            MediaCreate, PerMinute, 4, 20;
        );
        // Same goes for media
        let target_client_media = media_config! {
            ClientMediaConfig;

            download: PerMinute, ByteSize::mb(100), ByteSize::mb(50);
            upload: PerMinute, ByteSize::mb(10), ByteSize::mb(100);
            fetch: PerMinute, ByteSize::mb(100), ByteSize::mb(50);
        };

        // Currently, these values are completely arbitrary, not informed by any sort of
        // knowledge. In the future, it would be good to have some sort of analytics to
        // determine what some good defaults could be. Maybe getting some percentiles for
        // burst_capacity & timeframes used. How we'd tell the difference between power users
        // and malicilous attacks, I'm not sure.
        match preset {
            ConfigPreset::PrivateSmall => Self {
                target: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: target_client_map,
                        media: target_client_media,
                        additional_fields: AuthenticationFailures::new(
                            Timeframe::PerHour(nz(1)),
                            nz(20),
                        ),
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;

                            Join, PerHour, 10, 10;
                            Knock, PerHour, 10, 10;
                            Invite, PerHour, 10, 10;
                            MediaDownload, PerMinute, 10, 50;
                        ),
                        media: media_config! {
                            FederationMediaConfig;

                            download: PerMinute, ByteSize::mb(100), ByteSize::mb(100);
                        },
                        additional_fields: Nothing,
                    },
                },
                global: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            ClientRestriction;

                            Registration, PerDay, 10, 20;
                            Login, PerHour, 10, 10;
                            RegistrationTokenValidity, PerDay, 10, 20;
                            SendEvent, PerSecond, 2, 100;
                            Join, PerMinute, 1, 30;
                            Knock, PerMinute, 1, 30;
                            Invite, PerHour, 10, 20;
                            SendReport, PerHour, 1, 25;
                            CreateAlias, PerHour, 5, 20;
                            MediaDownload, PerMinute, 5, 150;
                            MediaCreate, PerMinute, 20, 50;
                        ),
                        media: media_config! {
                            ClientMediaConfig;

                            download: PerMinute, ByteSize::mb(250), ByteSize::mb(100);
                            upload: PerMinute, ByteSize::mb(50), ByteSize::mb(100);
                            fetch: PerMinute, ByteSize::mb(250), ByteSize::mb(100);
                        },
                        additional_fields: Nothing,
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;

                            Join, PerMinute, 10, 10;
                            Knock, PerMinute, 10, 10;
                            Invite, PerMinute, 10, 10;
                            MediaDownload, PerSecond, 10, 250;
                        ),
                        media: media_config! {
                            FederationMediaConfig;

                            download: PerMinute, ByteSize::mb(250), ByteSize::mb(250);
                        },
                        additional_fields: Nothing,
                    },
                },
            },
            ConfigPreset::PrivateMedium => Self {
                target: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: target_client_map,
                        media: target_client_media,
                        additional_fields: AuthenticationFailures::new(
                            Timeframe::PerHour(nz(10)),
                            nz(20),
                        ),
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;

                            Join, PerHour, 30, 10;
                            Knock, PerHour, 30, 10;
                            Invite, PerHour, 30, 10;
                            MediaDownload, PerMinute, 100, 50;
                        ),
                        media: media_config! {
                            FederationMediaConfig;

                            download: PerMinute, ByteSize::mb(200), ByteSize::mb(200);
                        },
                        additional_fields: Nothing,
                    },
                },
                global: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            ClientRestriction;

                            Registration, PerDay, 20, 20;
                            Login, PerHour, 25, 15;
                            RegistrationTokenValidity, PerDay, 20, 20;
                            SendEvent, PerSecond, 10, 100;
                            Join, PerMinute, 5, 30;
                            Knock, PerMinute, 5, 30;
                            Invite, PerMinute, 1, 20;
                            SendReport, PerHour, 10, 25;
                            CreateAlias, PerMinute, 1, 50;
                            MediaDownload, PerSecond, 1, 200;
                            MediaCreate, PerSecond, 2, 20;
                        ),
                        media: media_config! {
                            ClientMediaConfig;

                            download: PerMinute, ByteSize::mb(500), ByteSize::mb(200);
                            upload: PerMinute, ByteSize::mb(100), ByteSize::mb(200);
                            fetch: PerMinute, ByteSize::mb(500), ByteSize::mb(200);
                        },
                        additional_fields: Nothing,
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;

                            Join, PerMinute, 25, 25;
                            Knock, PerMinute, 25, 25;
                            Invite, PerMinute, 25, 25;
                            MediaDownload, PerSecond, 10, 100;
                        ),
                        media: media_config! {
                            FederationMediaConfig;

                            download: PerMinute, ByteSize::mb(500), ByteSize::mb(500);
                        },
                        additional_fields: Nothing,
                    },
                },
            },
            ConfigPreset::PublicMedium => Self {
                target: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: target_client_map,
                        media: target_client_media,
                        additional_fields: AuthenticationFailures::new(
                            Timeframe::PerHour(nz(10)),
                            nz(20),
                        ),
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;

                            Join, PerHour, 30, 10;
                            Knock, PerHour, 30, 10;
                            Invite, PerHour, 30, 10;
                            MediaDownload, PerMinute, 100, 50;
                        ),
                        media: media_config! {
                            FederationMediaConfig;

                            download: PerMinute, ByteSize::mb(200), ByteSize::mb(200);
                        },
                        additional_fields: Nothing,
                    },
                },
                global: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            ClientRestriction;

                            Registration, PerHour, 5, 20;
                            Login, PerHour, 25, 15;
                            // Public servers don't have registration tokens, so let's rate limit
                            // heavily so that if they revert to a private server again, it's a
                            // reminder to change their preset.
                            RegistrationTokenValidity, PerDay, 1, 1;
                            SendEvent, PerSecond, 10, 100;
                            Join, PerMinute, 5, 30;
                            Knock, PerMinute, 5, 30;
                            Invite, PerMinute, 1, 20;
                            SendReport, PerHour, 10, 25;
                            CreateAlias, PerMinute, 1, 50;
                            MediaDownload, PerSecond, 1, 200;
                            MediaCreate, PerSecond, 2, 20;
                        ),
                        media: media_config! {
                            ClientMediaConfig;

                            download: PerMinute, ByteSize::mb(500), ByteSize::mb(200);
                            upload: PerMinute, ByteSize::mb(100), ByteSize::mb(200);
                            fetch: PerMinute, ByteSize::mb(500), ByteSize::mb(200);
                        },
                        additional_fields: Nothing,
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;

                            Join, PerMinute, 25, 25;
                            Knock, PerMinute, 25, 25;
                            Invite, PerMinute, 25, 25;
                            MediaDownload, PerSecond, 10, 100;
                        ),
                        media: media_config! {
                            FederationMediaConfig;

                            download: PerMinute, ByteSize::mb(500), ByteSize::mb(500);
                        },
                        additional_fields: Nothing,
                    },
                },
            },
            ConfigPreset::PublicLarge => Self {
                target: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: target_client_map,
                        media: target_client_media,
                        additional_fields: AuthenticationFailures::new(
                            Timeframe::PerMinute(nz(1)),
                            nz(20),
                        ),
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;

                            Join, PerHour, 90, 30;
                            Knock, PerHour, 90, 30;
                            Invite, PerHour, 90, 30;
                            MediaDownload, PerMinute, 100, 50;
                        ),
                        media: media_config! {
                            FederationMediaConfig;

                            download: PerMinute, ByteSize::mb(600), ByteSize::mb(300);
                        },
                        additional_fields: Nothing,
                    },
                },
                global: ConfigFragment {
                    client: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            ClientRestriction;

                            Registration, PerMinute, 4, 25;
                            Login, PerMinute, 10, 25;
                            // Public servers don't have registration tokens, so let's rate limit
                            // heavily so that if they revert to a private server again, it's a
                            // reminder to change their preset.
                            RegistrationTokenValidity, PerDay, 1, 1;
                            SendEvent, PerSecond, 100, 50;
                            Join, PerSecond, 1, 20;
                            Knock, PerSecond, 1, 20;
                            Invite, PerMinute, 10, 40;
                            SendReport, PerMinute, 5, 25;
                            CreateAlias, PerMinute, 30, 20;
                            MediaDownload, PerSecond, 25, 200;
                            MediaCreate, PerSecond, 10, 30;
                        ),
                        media: media_config! {
                            ClientMediaConfig;

                            download: PerMinute, ByteSize::gb(2), ByteSize::mb(500);
                            upload: PerMinute, ByteSize::mb(500), ByteSize::mb(500);
                            fetch: PerMinute, ByteSize::gb(2), ByteSize::mb(500);
                        },
                        additional_fields: Nothing,
                    },
                    federation: ConfigFragmentFragment {
                        map: default_restriction_map!(
                            FederationRestriction;

                            Join, PerSecond, 1, 50;
                            Knock, PerSecond, 1, 50;
                            Invite, PerSecond, 1, 50;
                            MediaDownload, PerSecond, 50, 100;
                        ),
                        media: media_config! {
                            FederationMediaConfig;

                            download: PerMinute, ByteSize::gb(2), ByteSize::gb(1);
                        },
                        additional_fields: Nothing,
                    },
                },
            },
        }
    }
}
