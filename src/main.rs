#![warn(
    rust_2018_idioms,
    unused_qualifications,
    clippy::cloned_instead_of_copied,
    clippy::str_to_string
)]
#![allow(clippy::suspicious_else_formatting)]
#![deny(clippy::dbg_macro)]

use std::{future::Future, io, net::SocketAddr, sync::Arc, time::Duration};

use axum::{
    extract::{FromRequest, MatchedPath},
    handler::Handler,
    response::IntoResponse,
    routing::{get, on, MethodFilter},
    Router,
};
use axum_server::{bind, bind_rustls, tls_rustls::RustlsConfig, Handle as ServerHandle};
use conduit::api::{client_server, server_server};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use http::{
    header::{self, HeaderName},
    Method, Uri,
};
use opentelemetry::trace::{FutureExt, Tracer};
use ruma::api::{client::error::ErrorKind, IncomingRequest};
use tokio::{signal, sync::RwLock};
use tower::ServiceBuilder;
use tower_http::{
    cors::{self, CorsLayer},
    trace::TraceLayer,
    ServiceBuilderExt as _,
};
use tracing::{info, warn};
use tracing_subscriber::{prelude::*, EnvFilter};

pub use conduit::*; // Re-export everything from the library crate

#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(all(not(target_env = "msvc"), feature = "jemalloc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[tokio::main]
async fn main() {
    // Initialize DB
    let raw_config =
        Figment::new()
            .merge(
                Toml::file(Env::var("CONDUIT_CONFIG").expect(
                    "The CONDUIT_CONFIG env var needs to be set. Example: /etc/conduit.toml",
                ))
                .nested(),
            )
            .merge(Env::prefixed("CONDUIT_").global());

    let config = match raw_config.extract::<Config>() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("It looks like your config is invalid. The following error occured while parsing it: {}", e);
            std::process::exit(1);
        }
    };

    config.warn_deprecated();

    if let Err(e) = KeyValueDatabase::load_or_create(config).await {
        eprintln!(
            "The database couldn't be loaded or created. The following error occured: {}",
            e
        );
        std::process::exit(1);
    };

    let config = &services().globals.config;

    let start = async {
        run_server().await.unwrap();
    };

    if config.allow_jaeger {
        opentelemetry::global::set_text_map_propagator(opentelemetry_jaeger::Propagator::new());
        let tracer = opentelemetry_jaeger::new_pipeline()
            .install_batch(opentelemetry::runtime::Tokio)
            .unwrap();

        let span = tracer.start("conduit");
        start.with_current_context().await;
        drop(span);

        println!("exporting");
        opentelemetry::global::shutdown_tracer_provider();
    } else {
        let registry = tracing_subscriber::Registry::default();
        if config.tracing_flame {
            let (flame_layer, _guard) =
                tracing_flame::FlameLayer::with_file("./tracing.folded").unwrap();
            let flame_layer = flame_layer.with_empty_samples(false);

            let filter_layer = EnvFilter::new("trace,h2=off");

            let subscriber = registry.with(filter_layer).with(flame_layer);
            tracing::subscriber::set_global_default(subscriber).unwrap();
            start.await;
        } else {
            let fmt_layer = tracing_subscriber::fmt::Layer::new();
            let filter_layer = EnvFilter::try_new(&config.log)
                .or_else(|_| EnvFilter::try_new("info"))
                .unwrap();

            let subscriber = registry.with(filter_layer).with(fmt_layer);
            tracing::subscriber::set_global_default(subscriber).unwrap();
            start.await;
        }
    }
}

async fn run_server() -> io::Result<()> {
    let config = &services().globals.config;
    let addr = SocketAddr::from((config.address, config.port));

    let x_requested_with = HeaderName::from_static("x-requested-with");

    let middlewares = ServiceBuilder::new()
        .sensitive_headers([header::AUTHORIZATION])
        .layer(
            TraceLayer::new_for_http().make_span_with(|request: &http::Request<_>| {
                let path = if let Some(path) = request.extensions().get::<MatchedPath>() {
                    path.as_str()
                } else {
                    request.uri().path()
                };

                tracing::info_span!("http_request", %path)
            }),
        )
        .compression()
        .layer(
            CorsLayer::new()
                .allow_origin(cors::Any)
                .allow_methods([
                    Method::GET,
                    Method::POST,
                    Method::PUT,
                    Method::DELETE,
                    Method::OPTIONS,
                ])
                .allow_headers([
                    header::ORIGIN,
                    x_requested_with,
                    header::CONTENT_TYPE,
                    header::ACCEPT,
                    header::AUTHORIZATION,
                ])
                .max_age(Duration::from_secs(86400)),
        );

    let app = routes().layer(middlewares).into_make_service();
    let handle = ServerHandle::new();

    tokio::spawn(shutdown_signal(handle.clone()));

    match &config.tls {
        Some(tls) => {
            let conf = RustlsConfig::from_pem_file(&tls.certs, &tls.key).await?;
            bind_rustls(addr, conf).handle(handle).serve(app).await?;
        }
        None => {
            bind(addr).handle(handle).serve(app).await?;
        }
    }

    // On shutdown
    info!(target: "shutdown-sync", "Received shutdown notification, notifying sync helpers...");
    services().globals.rotate.fire();

    Ok(())
}

fn routes() -> Router {
    Router::new()
        .ruma_route(client_server::get_supported_versions_route)
        .ruma_route(client_server::get_register_available_route)
        .ruma_route(client_server::register_route)
        .ruma_route(client_server::get_login_types_route)
        .ruma_route(client_server::login_route)
        .ruma_route(client_server::whoami_route)
        .ruma_route(client_server::logout_route)
        .ruma_route(client_server::logout_all_route)
        .ruma_route(client_server::change_password_route)
        .ruma_route(client_server::deactivate_route)
        .ruma_route(client_server::third_party_route)
        .ruma_route(client_server::get_capabilities_route)
        .ruma_route(client_server::get_pushrules_all_route)
        .ruma_route(client_server::set_pushrule_route)
        .ruma_route(client_server::get_pushrule_route)
        .ruma_route(client_server::set_pushrule_enabled_route)
        .ruma_route(client_server::get_pushrule_enabled_route)
        .ruma_route(client_server::get_pushrule_actions_route)
        .ruma_route(client_server::set_pushrule_actions_route)
        .ruma_route(client_server::delete_pushrule_route)
        .ruma_route(client_server::get_room_event_route)
        .ruma_route(client_server::get_room_aliases_route)
        .ruma_route(client_server::get_filter_route)
        .ruma_route(client_server::create_filter_route)
        .ruma_route(client_server::set_global_account_data_route)
        .ruma_route(client_server::set_room_account_data_route)
        .ruma_route(client_server::get_global_account_data_route)
        .ruma_route(client_server::get_room_account_data_route)
        .ruma_route(client_server::set_displayname_route)
        .ruma_route(client_server::get_displayname_route)
        .ruma_route(client_server::set_avatar_url_route)
        .ruma_route(client_server::get_avatar_url_route)
        .ruma_route(client_server::get_profile_route)
        .ruma_route(client_server::set_presence_route)
        .ruma_route(client_server::get_presence_route)
        .ruma_route(client_server::upload_keys_route)
        .ruma_route(client_server::get_keys_route)
        .ruma_route(client_server::claim_keys_route)
        .ruma_route(client_server::create_backup_version_route)
        .ruma_route(client_server::update_backup_version_route)
        .ruma_route(client_server::delete_backup_version_route)
        .ruma_route(client_server::get_latest_backup_info_route)
        .ruma_route(client_server::get_backup_info_route)
        .ruma_route(client_server::add_backup_keys_route)
        .ruma_route(client_server::add_backup_keys_for_room_route)
        .ruma_route(client_server::add_backup_keys_for_session_route)
        .ruma_route(client_server::delete_backup_keys_for_room_route)
        .ruma_route(client_server::delete_backup_keys_for_session_route)
        .ruma_route(client_server::delete_backup_keys_route)
        .ruma_route(client_server::get_backup_keys_for_room_route)
        .ruma_route(client_server::get_backup_keys_for_session_route)
        .ruma_route(client_server::get_backup_keys_route)
        .ruma_route(client_server::set_read_marker_route)
        .ruma_route(client_server::create_receipt_route)
        .ruma_route(client_server::create_typing_event_route)
        .ruma_route(client_server::create_room_route)
        .ruma_route(client_server::redact_event_route)
        .ruma_route(client_server::report_event_route)
        .ruma_route(client_server::create_alias_route)
        .ruma_route(client_server::delete_alias_route)
        .ruma_route(client_server::get_alias_route)
        .ruma_route(client_server::join_room_by_id_route)
        .ruma_route(client_server::join_room_by_id_or_alias_route)
        .ruma_route(client_server::joined_members_route)
        .ruma_route(client_server::leave_room_route)
        .ruma_route(client_server::forget_room_route)
        .ruma_route(client_server::joined_rooms_route)
        .ruma_route(client_server::kick_user_route)
        .ruma_route(client_server::ban_user_route)
        .ruma_route(client_server::unban_user_route)
        .ruma_route(client_server::invite_user_route)
        .ruma_route(client_server::set_room_visibility_route)
        .ruma_route(client_server::get_room_visibility_route)
        .ruma_route(client_server::get_public_rooms_route)
        .ruma_route(client_server::get_public_rooms_filtered_route)
        .ruma_route(client_server::search_users_route)
        .ruma_route(client_server::get_member_events_route)
        .ruma_route(client_server::get_protocols_route)
        .ruma_route(client_server::send_message_event_route)
        .ruma_route(client_server::send_state_event_for_key_route)
        .ruma_route(client_server::get_state_events_route)
        .ruma_route(client_server::get_state_events_for_key_route)
        // Ruma doesn't have support for multiple paths for a single endpoint yet, and these routes
        // share one Ruma request / response type pair with {get,send}_state_event_for_key_route
        .route(
            "/_matrix/client/r0/rooms/:room_id/state/:event_type",
            get(client_server::get_state_events_for_empty_key_route)
                .put(client_server::send_state_event_for_empty_key_route),
        )
        .route(
            "/_matrix/client/v3/rooms/:room_id/state/:event_type",
            get(client_server::get_state_events_for_empty_key_route)
                .put(client_server::send_state_event_for_empty_key_route),
        )
        // These two endpoints allow trailing slashes
        .route(
            "/_matrix/client/r0/rooms/:room_id/state/:event_type/",
            get(client_server::get_state_events_for_empty_key_route)
                .put(client_server::send_state_event_for_empty_key_route),
        )
        .route(
            "/_matrix/client/v3/rooms/:room_id/state/:event_type/",
            get(client_server::get_state_events_for_empty_key_route)
                .put(client_server::send_state_event_for_empty_key_route),
        )
        .ruma_route(client_server::sync_events_route)
        .ruma_route(client_server::get_context_route)
        .ruma_route(client_server::get_message_events_route)
        .ruma_route(client_server::search_events_route)
        .ruma_route(client_server::turn_server_route)
        .ruma_route(client_server::send_event_to_device_route)
        .ruma_route(client_server::get_media_config_route)
        .ruma_route(client_server::create_content_route)
        .ruma_route(client_server::get_content_route)
        .ruma_route(client_server::get_content_as_filename_route)
        .ruma_route(client_server::get_content_thumbnail_route)
        .ruma_route(client_server::get_devices_route)
        .ruma_route(client_server::get_device_route)
        .ruma_route(client_server::update_device_route)
        .ruma_route(client_server::delete_device_route)
        .ruma_route(client_server::delete_devices_route)
        .ruma_route(client_server::get_tags_route)
        .ruma_route(client_server::update_tag_route)
        .ruma_route(client_server::delete_tag_route)
        .ruma_route(client_server::upload_signing_keys_route)
        .ruma_route(client_server::upload_signatures_route)
        .ruma_route(client_server::get_key_changes_route)
        .ruma_route(client_server::get_pushers_route)
        .ruma_route(client_server::set_pushers_route)
        // .ruma_route(client_server::third_party_route)
        .ruma_route(client_server::upgrade_room_route)
        .ruma_route(server_server::get_server_version_route)
        .route(
            "/_matrix/key/v2/server",
            get(server_server::get_server_keys_route),
        )
        .route(
            "/_matrix/key/v2/server/:key_id",
            get(server_server::get_server_keys_deprecated_route),
        )
        .ruma_route(server_server::get_public_rooms_route)
        .ruma_route(server_server::get_public_rooms_filtered_route)
        .ruma_route(server_server::send_transaction_message_route)
        .ruma_route(server_server::get_event_route)
        .ruma_route(server_server::get_missing_events_route)
        .ruma_route(server_server::get_event_authorization_route)
        .ruma_route(server_server::get_room_state_route)
        .ruma_route(server_server::get_room_state_ids_route)
        .ruma_route(server_server::create_join_event_template_route)
        .ruma_route(server_server::create_join_event_v1_route)
        .ruma_route(server_server::create_join_event_v2_route)
        .ruma_route(server_server::create_invite_route)
        .ruma_route(server_server::get_devices_route)
        .ruma_route(server_server::get_room_information_route)
        .ruma_route(server_server::get_profile_information_route)
        .ruma_route(server_server::get_keys_route)
        .ruma_route(server_server::claim_keys_route)
        .fallback(not_found.into_service())
}

async fn shutdown_signal(handle: ServerHandle) {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    let sig: &str;

    tokio::select! {
        _ = ctrl_c => { sig = "Ctrl+C"; },
        _ = terminate => { sig = "SIGTERM"; },
    }

    warn!("Received {}, shutting down...", sig);
    handle.graceful_shutdown(Some(Duration::from_secs(30)));
}

async fn not_found(_uri: Uri) -> impl IntoResponse {
    Error::BadRequest(ErrorKind::NotFound, "Unknown or unimplemented route")
}

trait RouterExt {
    fn ruma_route<H, T>(self, handler: H) -> Self
    where
        H: RumaHandler<T>,
        T: 'static;
}

impl RouterExt for Router {
    fn ruma_route<H, T>(self, handler: H) -> Self
    where
        H: RumaHandler<T>,
        T: 'static,
    {
        handler.add_to_router(self)
    }
}

pub trait RumaHandler<T> {
    // Can't transform to a handler without boxing or relying on the nightly-only
    // impl-trait-in-traits feature. Moving a small amount of extra logic into the trait
    // allows bypassing both.
    fn add_to_router(self, router: Router) -> Router;
}

macro_rules! impl_ruma_handler {
    ( $($ty:ident),* $(,)? ) => {
        #[axum::async_trait]
        #[allow(non_snake_case)]
        impl<Req, E, F, Fut, $($ty,)*> RumaHandler<($($ty,)* Ruma<Req>,)> for F
        where
            Req: IncomingRequest + Send + 'static,
            F: FnOnce($($ty,)* Ruma<Req>) -> Fut + Clone + Send + 'static,
            Fut: Future<Output = Result<Req::OutgoingResponse, E>>
                + Send,
            E: IntoResponse,
            $( $ty: FromRequest<axum::body::Body> + Send + 'static, )*
        {
            fn add_to_router(self, mut router: Router) -> Router {
                let meta = Req::METADATA;
                let method_filter = method_to_filter(meta.method);

                for path in IntoIterator::into_iter([meta.unstable_path, meta.r0_path, meta.stable_path]).flatten() {
                    let handler = self.clone();

                    router = router.route(path, on(method_filter, |$( $ty: $ty, )* req| async move {
                        handler($($ty,)* req).await.map(RumaResponse)
                    }))
                }

                router
            }
        }
    };
}

impl_ruma_handler!();
impl_ruma_handler!(T1);
impl_ruma_handler!(T1, T2);
impl_ruma_handler!(T1, T2, T3);
impl_ruma_handler!(T1, T2, T3, T4);
impl_ruma_handler!(T1, T2, T3, T4, T5);
impl_ruma_handler!(T1, T2, T3, T4, T5, T6);
impl_ruma_handler!(T1, T2, T3, T4, T5, T6, T7);
impl_ruma_handler!(T1, T2, T3, T4, T5, T6, T7, T8);

fn method_to_filter(method: Method) -> MethodFilter {
    let method_filter = match method {
        Method::DELETE => MethodFilter::DELETE,
        Method::GET => MethodFilter::GET,
        Method::HEAD => MethodFilter::HEAD,
        Method::OPTIONS => MethodFilter::OPTIONS,
        Method::PATCH => MethodFilter::PATCH,
        Method::POST => MethodFilter::POST,
        Method::PUT => MethodFilter::PUT,
        Method::TRACE => MethodFilter::TRACE,
        m => panic!("Unsupported HTTP method: {:?}", m),
    };
    method_filter
}
