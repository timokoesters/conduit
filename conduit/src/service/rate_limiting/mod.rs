use std::{
    collections::{HashMap, hash_map::Entry},
    net::IpAddr,
    sync::Arc,
    time::Duration,
};

use conduit_config::{
    Config,
    rate_limiting::{MediaLimitation, RequestLimitation, Restriction},
};
use ruma::{
    OwnedServerName, OwnedUserId, UserId,
    api::{
        Metadata,
        client::error::{ErrorKind, RetryAfter},
    },
};
use tokio::{
    sync::{Mutex, MutexGuard, RwLock},
    time::Instant,
};

use crate::{Error, Result, service::appservice::RegistrationInfo, services};

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

// NOTE: still chokes on the global mutex around the map, which in theory only needed for inserts,
// but due to rust ownership model I don't think it's possible to (easily) only require to lock
// said mutex for inserts (and have a rwlock for removals if memory usage ever becomes a problem).
type MediaBucket = Mutex<HashMap<Target, Arc<Mutex<Instant>>>>;
type GlobalMediaBucket = Arc<Mutex<Instant>>;

type RequestPermittedAfter = Arc<Mutex<Instant>>;

pub struct Service {
    buckets: Mutex<HashMap<(Target, Restriction), RequestPermittedAfter>>,
    global_bucket: Mutex<HashMap<Restriction, RequestPermittedAfter>>,

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
            .additional_fields
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
