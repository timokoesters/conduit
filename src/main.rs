#![warn(rust_2018_idioms)]
#![allow(clippy::suspicious_else_formatting)]
#![deny(clippy::dbg_macro)]

pub mod appservice_server;
pub mod client_server;
pub mod server_server;

mod database;
mod error;
mod pdu;
mod ruma_wrapper;
mod utils;

use std::sync::Arc;

use database::Config;
pub use database::Database;
pub use error::{Error, Result};
pub use pdu::PduEvent;
pub use rocket::State;
use ruma::api::client::error::ErrorKind;
pub use ruma_wrapper::{ConduitResult, Ruma, RumaResponse};

use rocket::{
    catch, catchers,
    figment::{
        providers::{Env, Format, Toml},
        Figment,
    },
    routes, Request,
};
use tokio::sync::RwLock;
use tracing::span;
use tracing_subscriber::{prelude::*, Registry};

fn setup_rocket(config: Figment, data: Arc<RwLock<Database>>) -> rocket::Rocket<rocket::Build> {
    rocket::custom(config)
        .manage(data)
        .mount(
            "/",
            routes![
                client_server::get_supported_versions_route,
                client_server::get_register_available_route,
                client_server::register_route,
                client_server::get_login_types_route,
                client_server::login_route,
                client_server::whoami_route,
                client_server::logout_route,
                client_server::logout_all_route,
                client_server::change_password_route,
                client_server::deactivate_route,
                client_server::get_capabilities_route,
                client_server::get_pushrules_all_route,
                client_server::set_pushrule_route,
                client_server::get_pushrule_route,
                client_server::set_pushrule_enabled_route,
                client_server::get_pushrule_enabled_route,
                client_server::get_pushrule_actions_route,
                client_server::set_pushrule_actions_route,
                client_server::delete_pushrule_route,
                client_server::get_room_event_route,
                client_server::get_filter_route,
                client_server::create_filter_route,
                client_server::set_global_account_data_route,
                client_server::set_room_account_data_route,
                client_server::get_global_account_data_route,
                client_server::get_room_account_data_route,
                client_server::set_displayname_route,
                client_server::get_displayname_route,
                client_server::set_avatar_url_route,
                client_server::get_avatar_url_route,
                client_server::get_profile_route,
                client_server::set_presence_route,
                client_server::get_presence_route,
                client_server::upload_keys_route,
                client_server::get_keys_route,
                client_server::claim_keys_route,
                client_server::create_backup_route,
                client_server::update_backup_route,
                client_server::delete_backup_route,
                client_server::get_latest_backup_route,
                client_server::get_backup_route,
                client_server::add_backup_key_sessions_route,
                client_server::add_backup_keys_route,
                client_server::delete_backup_key_session_route,
                client_server::delete_backup_key_sessions_route,
                client_server::delete_backup_keys_route,
                client_server::get_backup_key_session_route,
                client_server::get_backup_key_sessions_route,
                client_server::get_backup_keys_route,
                client_server::set_read_marker_route,
                client_server::create_receipt_route,
                client_server::create_typing_event_route,
                client_server::create_room_route,
                client_server::redact_event_route,
                client_server::create_alias_route,
                client_server::delete_alias_route,
                client_server::get_alias_route,
                client_server::join_room_by_id_route,
                client_server::join_room_by_id_or_alias_route,
                client_server::joined_members_route,
                client_server::leave_room_route,
                client_server::forget_room_route,
                client_server::joined_rooms_route,
                client_server::kick_user_route,
                client_server::ban_user_route,
                client_server::unban_user_route,
                client_server::invite_user_route,
                client_server::set_room_visibility_route,
                client_server::get_room_visibility_route,
                client_server::get_public_rooms_route,
                client_server::get_public_rooms_filtered_route,
                client_server::search_users_route,
                client_server::get_member_events_route,
                client_server::get_protocols_route,
                client_server::send_message_event_route,
                client_server::send_state_event_for_key_route,
                client_server::send_state_event_for_empty_key_route,
                client_server::get_state_events_route,
                client_server::get_state_events_for_key_route,
                client_server::get_state_events_for_empty_key_route,
                client_server::sync_events_route,
                client_server::get_context_route,
                client_server::get_message_events_route,
                client_server::search_events_route,
                client_server::turn_server_route,
                client_server::send_event_to_device_route,
                client_server::get_media_config_route,
                client_server::create_content_route,
                client_server::get_content_route,
                client_server::get_content_thumbnail_route,
                client_server::get_devices_route,
                client_server::get_device_route,
                client_server::update_device_route,
                client_server::delete_device_route,
                client_server::delete_devices_route,
                client_server::get_tags_route,
                client_server::update_tag_route,
                client_server::delete_tag_route,
                client_server::options_route,
                client_server::upload_signing_keys_route,
                client_server::upload_signatures_route,
                client_server::get_key_changes_route,
                client_server::get_pushers_route,
                client_server::set_pushers_route,
                // client_server::third_party_route,
                client_server::upgrade_room_route,
                server_server::get_server_version_route,
                server_server::get_server_keys_route,
                server_server::get_server_keys_deprecated_route,
                server_server::get_public_rooms_route,
                server_server::get_public_rooms_filtered_route,
                server_server::send_transaction_message_route,
                server_server::get_event_route,
                server_server::get_missing_events_route,
                server_server::get_event_authorization_route,
                server_server::get_room_state_route,
                server_server::get_room_state_ids_route,
                server_server::create_join_event_template_route,
                server_server::create_join_event_route,
                server_server::create_invite_route,
                server_server::get_devices_route,
                server_server::get_room_information_route,
                server_server::get_profile_information_route,
                server_server::get_keys_route,
                server_server::claim_keys_route,
            ],
        )
        .register(
            "/",
            catchers![
                not_found_catcher,
                forbidden_catcher,
                unknown_token_catcher,
                missing_token_catcher,
                bad_json_catcher
            ],
        )
}

#[rocket::main]
async fn main() {
    // Force log level off, so we can use our own logger
    std::env::set_var("CONDUIT_LOG_LEVEL", "off");

    let raw_config =
        Figment::from(rocket::Config::release_default())
            .merge(
                Toml::file(Env::var("CONDUIT_CONFIG").expect(
                    "The CONDUIT_CONFIG env var needs to be set. Example: /etc/conduit.toml",
                ))
                .nested(),
            )
            .merge(Env::prefixed("CONDUIT_").global());

    let config = raw_config
        .extract::<Config>()
        .expect("It looks like your config is invalid. Please take a look at the error");

    let db = Database::load_or_create(config.clone())
        .await
        .expect("config is valid");

    if config.allow_jaeger {
        let (tracer, _uninstall) = opentelemetry_jaeger::new_pipeline()
            .with_service_name("conduit")
            .install()
            .unwrap();
        let telemetry = tracing_opentelemetry::layer().with_tracer(tracer);
        Registry::default().with(telemetry).try_init().unwrap();

        let root = span!(tracing::Level::INFO, "app_start", work_units = 2);
        let _enter = root.enter();

        let rocket = setup_rocket(raw_config, db);
        rocket.launch().await.unwrap();
    } else {
        std::env::set_var("RUST_LOG", config.log);
        tracing_subscriber::fmt::init();

        let rocket = setup_rocket(raw_config, db);
        rocket.launch().await.unwrap();
    }
}

#[catch(404)]
fn not_found_catcher(_: &Request<'_>) -> String {
    "404 Not Found".to_owned()
}

#[catch(580)]
fn forbidden_catcher() -> Result<()> {
    Err(Error::BadRequest(ErrorKind::Forbidden, "Forbidden."))
}

#[catch(581)]
fn unknown_token_catcher() -> Result<()> {
    Err(Error::BadRequest(
        ErrorKind::UnknownToken { soft_logout: false },
        "Unknown token.",
    ))
}

#[catch(582)]
fn missing_token_catcher() -> Result<()> {
    Err(Error::BadRequest(ErrorKind::MissingToken, "Missing token."))
}

#[catch(583)]
fn bad_json_catcher() -> Result<()> {
    Err(Error::BadRequest(ErrorKind::BadJson, "Bad json."))
}
