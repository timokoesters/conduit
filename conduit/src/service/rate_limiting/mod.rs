use std::{
    collections::{hash_map::Entry, HashMap},
    net::IpAddr,
    sync::Arc,
    time::Duration,
};

use ruma::{
    api::{
        client::error::{ErrorKind, RetryAfter},
        federation::membership::create_knock_event,
        Metadata,
    },
    OwnedServerName, OwnedUserId, UserId,
};
use serde::Deserialize;
use tokio::{
    sync::{Mutex, MutexGuard, RwLock},
    time::Instant,
};

use crate::{
    config::rate_limiting::{MediaLimitation, RequestLimitation},
    service::appservice::RegistrationInfo,
    services, Config, Error, Result,
};

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Target {
    User(OwnedUserId),
    // Server endpoints should be rate-limited on a server and room basis
    Server(OwnedServerName),
    Appservice { id: String, rate_limited: bool },
    Ip(IpAddr),
}

impl Target {
    pub fn from_client_request(
        registration_info: Option<RegistrationInfo>,
        sender_user: &UserId,
    ) -> Self {
        if let Some(info) = registration_info {
            // `rate_limited` only effects "masqueraded users", "The sender [user?] is excluded"
            return Target::Appservice {
                id: info.registration.id,
                rate_limited: info.registration.rate_limited.unwrap_or(true)
                    && !(sender_user.server_name() == services().globals.server_name()
                        && info.registration.sender_localpart == sender_user.localpart()),
            };
        }

        Target::User(sender_user.to_owned())
    }

    pub fn from_client_request_optional_auth(
        registration_info: Option<RegistrationInfo>,
        sender_user: &Option<OwnedUserId>,
        ip_addr: Option<IpAddr>,
    ) -> Option<Self> {
        if let Some(sender_user) = sender_user.as_ref() {
            Some(Self::from_client_request(registration_info, sender_user))
        } else {
            ip_addr.map(Self::Ip)
        }
    }

    fn rate_limited(&self) -> bool {
        match self {
            Target::User(user_id) => user_id != services().globals.server_user(),
            Target::Appservice {
                id: _,
                rate_limited,
            } => *rate_limited,
            _ => true,
        }
    }

    pub fn is_authenticated(&self) -> bool {
        !matches!(self, Target::Ip(_))
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Restriction {
    Client(ClientRestriction),
    Federation(FederationRestriction),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum ClientRestriction {
    Registration,
    Login,
    RegistrationTokenValidity,

    SendEvent,

    Join,
    Invite,
    Knock,

    SendReport,
    CreateAlias,

    MediaDownload,
    MediaCreate,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[serde(rename_all = "snake_case")]
pub enum FederationRestriction {
    Join,
    Knock,
    Invite,

    // Transactions should be handled by a completely dedicated rate-limiter
    Transaction,

    MediaDownload,
}

impl TryFrom<Metadata> for Restriction {
    type Error = ();

    fn try_from(value: Metadata) -> Result<Self, Self::Error> {
        use ruma::api::{
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
                membership::{create_invite, create_join_event},
            },
            IncomingRequest,
        };
        use Restriction::*;

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

type MediaBucket = Mutex<HashMap<Target, Arc<Mutex<Instant>>>>;
type GlobalMediaBucket = Arc<Mutex<Instant>>;

pub struct Service {
    buckets: Mutex<HashMap<(Target, Restriction), Arc<Mutex<Instant>>>>,
    global_bucket: Mutex<HashMap<Restriction, Arc<Mutex<Instant>>>>,

    media_upload: MediaBucket,
    media_fetch: MediaBucket,
    media_download: MediaBucket,

    global_media_upload: GlobalMediaBucket,
    global_media_fetch: GlobalMediaBucket,
    global_media_download_client: GlobalMediaBucket,
    global_media_download_federation: GlobalMediaBucket,

    authentication_failures: RwLock<HashMap<IpAddr, Arc<RwLock<Instant>>>>,
}

impl Service {
    pub fn build(config: &Config) -> Arc<Self> {
        let now = Instant::now();
        let global_media_config = &config.rate_limiting.global;

        Arc::new(Self {
            buckets: Mutex::new(HashMap::new()),
            global_bucket: Mutex::new(HashMap::new()),

            media_upload: Mutex::new(HashMap::new()),
            media_fetch: Mutex::new(HashMap::new()),
            media_download: Mutex::new(HashMap::new()),

            global_media_upload: default_media_entry(global_media_config.client.media.upload, now),
            global_media_fetch: default_media_entry(global_media_config.client.media.fetch, now),
            global_media_download_client: default_media_entry(
                global_media_config.client.media.download,
                now,
            ),
            global_media_download_federation: default_media_entry(
                global_media_config.federation.media.download,
                now,
            ),

            authentication_failures: RwLock::new(HashMap::new()),
        })
    }

    //TODO: use checked and saturating arithmetic

    /// Takes the target and request, and either accepts the request while adding to the
    /// bucket, or rejects the request, returning the duration that should be waited until
    /// the request should be retried.
    pub async fn check(&self, target: Option<Target>, request: Metadata) -> Result<()> {
        let Ok(restriction) = request.try_into() else {
            // Endpoint has no associated restriction
            return Ok(());
        };
        let arrival = Instant::now();

        let config = services()
            .globals
            .config
            .rate_limiting
            .global
            .get(&restriction);

        let mut map = self.global_bucket.lock().await;

        let entry = map.entry(restriction);
        let proposed_entry = match &entry {
            Entry::Occupied(occupied_entry) => {
                let entry = Arc::clone(occupied_entry.get());
                let entry = entry.lock().await;

                if arrival.checked_duration_since(*entry).is_none() {
                    return instant_to_err(&entry);
                }

                let min_instant = arrival
                    - Duration::from_nanos(
                        config.timeframe.nano_gap() * config.burst_capacity.get(),
                    );
                entry.max(min_instant) + Duration::from_nanos(config.timeframe.nano_gap())
            }
            Entry::Vacant(_) => {
                arrival
                    - Duration::from_nanos(
                        config.timeframe.nano_gap() * (config.burst_capacity.get() - 1),
                    )
            }
        };

        if let Some(target) = target {
            let config = services()
                .globals
                .config
                .rate_limiting
                .target
                .get(&restriction);

            let mut map = self.buckets.lock().await;
            let entry = map.entry((target, restriction));
            match entry {
                Entry::Occupied(occupied_entry) => {
                    let entry = Arc::clone(occupied_entry.get());
                    let mut entry = entry.lock().await;

                    if arrival.checked_duration_since(*entry).is_none() {
                        return instant_to_err(&entry);
                    }

                    let min_instant = arrival
                        - Duration::from_nanos(
                            config.timeframe.nano_gap() * config.burst_capacity.get(),
                        );
                    *entry =
                        entry.max(min_instant) + Duration::from_nanos(config.timeframe.nano_gap());
                }
                Entry::Vacant(vacant_entry) => {
                    vacant_entry.insert(Arc::new(Mutex::new(
                        arrival
                            - Duration::from_nanos(
                                config.timeframe.nano_gap() * (config.burst_capacity.get() - 1),
                            ),
                    )));
                }
            }
        }

        entry.insert_entry(Arc::new(Mutex::new(proposed_entry)));

        Ok(())
    }

    pub async fn check_media_download(&self, target: Option<Target>, size: u64) -> Result<()> {
        // All targets besides servers use the client-server API
        let (target_limitation, global_limitation, global_bucket) =
            if let Some(Target::Server(_)) = &target {
                (
                    services()
                        .globals
                        .config
                        .rate_limiting
                        .target
                        .federation
                        .media
                        .download,
                    services()
                        .globals
                        .config
                        .rate_limiting
                        .global
                        .federation
                        .media
                        .download,
                    &self.global_media_download_federation,
                )
            } else {
                (
                    services()
                        .globals
                        .config
                        .rate_limiting
                        .target
                        .client
                        .media
                        .download,
                    services()
                        .globals
                        .config
                        .rate_limiting
                        .global
                        .client
                        .media
                        .download,
                    &self.global_media_download_client,
                )
            };

        check_media(
            target,
            size,
            target_limitation,
            global_limitation,
            &self.media_download,
            global_bucket,
        )
        .await
    }

    pub async fn check_media_upload(&self, target: Target, size: u64) -> Result<()> {
        let target_limitation = services()
            .globals
            .config
            .rate_limiting
            .target
            // Media can only be uploaded on the client-server API
            .client
            .media
            .upload;

        let global_limitation = services()
            .globals
            .config
            .rate_limiting
            .global
            // Media can only be uploaded on the client-server API
            .client
            .media
            .upload;

        check_media(
            Some(target),
            size,
            target_limitation,
            global_limitation,
            &self.media_upload,
            &self.global_media_upload,
        )
        .await
    }

    pub async fn check_media_pre_fetch(&self, target: &Target) -> Result<()> {
        if !target.rate_limited() {
            return Ok(());
        }

        let arrival = Instant::now();

        let check = async |map: &MediaBucket, global_bucket: &GlobalMediaBucket| {
            let map = map.lock().await;
            if let Some(mutex) = map.get(target) {
                let mutex = mutex.lock().await;

                if arrival.checked_duration_since(*mutex).is_none() {
                    return instant_to_err(&mutex);
                }
            }

            let global_bucket = global_bucket.lock().await;

            if arrival.checked_duration_since(*global_bucket).is_none() {
                return instant_to_err(&global_bucket);
            }

            Ok(())
        };

        // checking fetch
        check(&self.media_fetch, &self.global_media_fetch).await?;

        // checking download as well
        check(&self.media_download, &self.global_media_download_client).await
    }

    /// Checks whether the ip address is has been rate limited due to too many bad access tokens being sent.
    pub async fn pre_auth_check(&self, ip_addr: IpAddr) -> Result<()> {
        let arrival = Instant::now();

        if let Some(instant) = self.authentication_failures.read().await.get(&ip_addr) {
            let instant = instant.read().await;

            if arrival.checked_duration_since(*instant).is_none() {
                return instant_to_err(&instant);
            }
        }

        Ok(())
    }

    /// Updates the bad auth rate limiter when a bad access token is sent where access tokens auth is an option.
    pub async fn update_post_auth_failure(&self, ip_addr: IpAddr) {
        let arrival = Instant::now();

        let RequestLimitation {
            timeframe,
            burst_capacity,
        } = services()
            .globals
            .config
            .rate_limiting
            .target
            .client
            .authentication_failures;

        let mut map = self.authentication_failures.write().await;
        let entry = map.entry(ip_addr);

        match entry {
            Entry::Occupied(occupied_entry) => {
                let entry = Arc::clone(occupied_entry.get());
                let mut entry = entry.write().await;

                let min_instant =
                    arrival - Duration::from_nanos(timeframe.nano_gap() * burst_capacity.get());
                *entry = entry.max(min_instant) + Duration::from_nanos(timeframe.nano_gap());
            }
            Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(Arc::new(RwLock::new(
                    arrival - Duration::from_nanos(burst_capacity.get() / timeframe.nano_gap()),
                )));
            }
        }
    }

    pub async fn update_media_post_fetch(&self, target: Target, size: u64) {
        if !target.rate_limited() {
            return;
        }

        let arrival = Instant::now();

        let update = async |map: &MediaBucket,
                            target_limitation: &MediaLimitation,
                            global_bucket: &GlobalMediaBucket,
                            global_limitation: &MediaLimitation| {
            let mut map = map.lock().await;
            let entry = map.entry(target.clone());

            match entry {
                Entry::Occupied(occupied_entry) => {
                    let entry = Arc::clone(occupied_entry.get());

                    let _ =
                        update_media_entry(size, target_limitation, &arrival, entry, false).await;
                }
                Entry::Vacant(vacant_entry) => {
                    vacant_entry.insert(Arc::new(Mutex::new(
                        arrival
                            - Duration::from_nanos(
                                target_limitation.burst_capacity.as_u64()
                                    / target_limitation.timeframe.bytes_per_sec(),
                            ),
                    )));
                }
            }

            let _ = update_media_entry(
                size,
                global_limitation,
                &arrival,
                Arc::clone(global_bucket),
                false,
            )
            .await;
        };

        // updating fetch
        update(
            &self.media_fetch,
            &services()
                .globals
                .config
                .rate_limiting
                .target
                .client
                .media
                .fetch,
            &self.global_media_fetch,
            &services()
                .globals
                .config
                .rate_limiting
                .global
                .client
                .media
                .fetch,
        )
        .await;

        // updating download as well
        update(
            &self.media_download,
            &services()
                .globals
                .config
                .rate_limiting
                .target
                .client
                .media
                .download,
            &self.global_media_download_client,
            &services()
                .globals
                .config
                .rate_limiting
                .global
                .client
                .media
                .download,
        )
        .await;
    }
}

async fn update_media_entry(
    size: u64,
    limitation: &MediaLimitation,
    arrival: &Instant,
    entry: Arc<Mutex<Instant>>,
    and_check: bool,
) -> Result<()> {
    let mut entry = entry.lock().await;

    //TODO: use more precise conversion than secs
    let proposed_entry = get_proposed_entry(size, limitation, arrival, &entry, and_check)?;

    *entry = proposed_entry;

    Ok(())
}

fn get_proposed_entry(
    size: u64,
    limitation: &MediaLimitation,
    arrival: &Instant,
    entry: &MutexGuard<'_, Instant>,
    and_check: bool,
) -> Result<Instant> {
    let min_instant = *arrival
        - Duration::from_secs(
            limitation.burst_capacity.as_u64() / limitation.timeframe.bytes_per_sec(),
        );

    let proposed_entry =
        entry.max(min_instant) + Duration::from_secs(size / limitation.timeframe.bytes_per_sec());

    if and_check && arrival.checked_duration_since(proposed_entry).is_none() {
        return instant_to_err(&proposed_entry).map(|_| proposed_entry);
    }

    Ok(proposed_entry)
}

async fn check_media(
    target: Option<Target>,
    size: u64,
    target_limitation: MediaLimitation,
    global_limitation: MediaLimitation,
    target_map: &MediaBucket,
    global_bucket: &GlobalMediaBucket,
) -> Result<()> {
    if !target.as_ref().is_some_and(Target::rate_limited) {
        return Ok(());
    }

    let arrival = Instant::now();

    let mut global_bucket = global_bucket.lock().await;
    let proposed = get_proposed_entry(size, &global_limitation, &arrival, &global_bucket, true)?;

    if let Some(target) = target {
        let mut map = target_map.lock().await;
        let entry = map.entry(target);

        match entry {
            Entry::Occupied(occupied_entry) => {
                let entry = Arc::clone(occupied_entry.get());

                update_media_entry(size, &target_limitation, &arrival, entry, true).await?;
            }
            Entry::Vacant(vacant_entry) => {
                vacant_entry.insert(default_media_entry(target_limitation, arrival));
            }
        }
    }

    *global_bucket = proposed;

    Ok(())
}

fn default_media_entry(
    target_limitation: MediaLimitation,
    arrival: Instant,
) -> Arc<Mutex<Instant>> {
    Arc::new(Mutex::new(
        arrival
            - Duration::from_nanos(
                target_limitation.burst_capacity.as_u64()
                    / target_limitation.timeframe.bytes_per_sec(),
            ),
    ))
}

fn instant_to_err(instant: &Instant) -> Result<()> {
    let now = Instant::now();

    Err(Error::BadRequest(
        ErrorKind::LimitExceeded {
            // Not using ::DateTime because conversion from Instant to SystemTime is convoluted
            retry_after: Some(RetryAfter::Delay(instant.duration_since(now))),
        },
        "Rate limit exceeded",
    ))
}
