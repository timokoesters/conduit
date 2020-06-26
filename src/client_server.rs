use std::{
    collections::BTreeMap,
    convert::{TryFrom, TryInto},
    time::{Duration, SystemTime},
};

use crate::{utils, ConduitResult, Database, Error, Ruma};
use keys::{upload_signatures, upload_signing_keys};
use log::warn;
use rocket::{delete, get, options, post, put, State};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::{
            account::{get_username_availability, register},
            alias::{create_alias, delete_alias, get_alias},
            backup::{
                add_backup_keys, create_backup, get_backup, get_backup_keys, get_latest_backup,
                update_backup,
            },
            capabilities::get_capabilities,
            config::{get_global_account_data, set_global_account_data},
            context::get_context,
            device::{self, delete_device, delete_devices, get_device, get_devices, update_device},
            directory::{
                self, get_public_rooms, get_public_rooms_filtered, get_room_visibility,
                set_room_visibility,
            },
            filter::{self, create_filter, get_filter},
            keys::{self, claim_keys, get_keys, upload_keys},
            media::{create_content, get_content, get_content_thumbnail, get_media_config},
            membership::{
                ban_user, forget_room, get_member_events, invite_user, join_room_by_id,
                join_room_by_id_or_alias, kick_user, leave_room, unban_user,
            },
            message::{create_message_event, get_message_events},
            presence::set_presence,
            profile::{
                get_avatar_url, get_display_name, get_profile, set_avatar_url, set_display_name,
            },
            push::{get_pushers, get_pushrules_all, set_pushrule, set_pushrule_enabled},
            read_marker::set_read_marker,
            redact::redact_event,
            room::{self, create_room},
            session::{get_login_types, login, logout},
            state::{
                create_state_event_for_empty_key, create_state_event_for_key, get_state_events,
                get_state_events_for_empty_key, get_state_events_for_key,
            },
            sync::sync_events,
            thirdparty::get_protocols,
            to_device::{self, send_event_to_device},
            typing::create_typing_event,
            uiaa::{AuthFlow, UiaaInfo},
            user_directory::search_users,
        },
        unversioned::get_supported_versions,
    },
    events::{
        collections::only::Event as EduEvent,
        room::{
            canonical_alias, guest_access, history_visibility, join_rules, member, name, redaction,
            topic,
        },
        EventJson, EventType,
    },
    identifiers::{RoomAliasId, RoomId, RoomVersionId, UserId},
};
use serde_json::json;

const GUEST_NAME_LENGTH: usize = 10;
const DEVICE_ID_LENGTH: usize = 10;
const TOKEN_LENGTH: usize = 256;
const MXC_LENGTH: usize = 256;
const SESSION_ID_LENGTH: usize = 256;

#[get("/_matrix/client/versions")]
pub fn get_supported_versions_route() -> ConduitResult<get_supported_versions::Response> {
    let mut unstable_features = BTreeMap::new();

    unstable_features.insert("org.matrix.e2e_cross_signing".to_owned(), true);

    Ok(get_supported_versions::Response {
        versions: vec!["r0.5.0".to_owned(), "r0.6.0".to_owned()],
        unstable_features,
    }
    .into())
}

#[get("/_matrix/client/r0/register/available", data = "<body>")]
pub fn get_register_available_route(
    db: State<'_, Database>,
    body: Ruma<get_username_availability::Request>,
) -> ConduitResult<get_username_availability::Response> {
    // Validate user id
    let user_id = UserId::parse_with_server_name(body.username.clone(), db.globals.server_name())
        .ok()
        .filter(|user_id| {
            !user_id.is_historical() && user_id.server_name() == db.globals.server_name()
        })
        .ok_or(Error::BadRequest(
            ErrorKind::InvalidUsername,
            "Username is invalid.",
        ))?;

    // Check if username is creative enough
    if db.users.exists(&user_id)? {
        return Err(Error::BadRequest(
            ErrorKind::UserInUse,
            "Desired user ID is already taken.",
        ));
    }

    // TODO add check for appservice namespaces

    // If no if check is true we have an username that's available to be used.
    Ok(get_username_availability::Response { available: true }.into())
}

#[post("/_matrix/client/r0/register", data = "<body>")]
pub fn register_route(
    db: State<'_, Database>,
    body: Ruma<register::Request>,
) -> ConduitResult<register::Response> {
    if db.globals.registration_disabled() {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "Registration has been disabled.",
        ));
    }

    // Validate user id
    let user_id = UserId::parse_with_server_name(
        body.username
            .clone()
            .unwrap_or_else(|| utils::random_string(GUEST_NAME_LENGTH))
            .to_lowercase(),
        db.globals.server_name(),
    )
    .ok()
    .filter(|user_id| !user_id.is_historical() && user_id.server_name() == db.globals.server_name())
    .ok_or(Error::BadRequest(
        ErrorKind::InvalidUsername,
        "Username is invalid.",
    ))?;

    // Check if username is creative enough
    if db.users.exists(&user_id)? {
        return Err(Error::BadRequest(
            ErrorKind::UserInUse,
            "Desired user ID is already taken.",
        ));
    }

    // UIAA
    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec!["m.login.dummy".to_owned()],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) =
            db.uiaa
                .try_auth(&user_id, "", auth, &uiaainfo, &db.users, &db.globals)?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa.create(&user_id, "", &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
    }

    let password = body.password.clone().unwrap_or_default();

    if let Ok(hash) = utils::calculate_hash(&password) {
        // Create user
        db.users.create(&user_id, &hash)?;
    } else {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Password does not meet the requirements.",
        ));
    }

    // Generate new device id if the user didn't specify one
    let device_id = body
        .device_id
        .clone()
        .unwrap_or_else(|| utils::random_string(DEVICE_ID_LENGTH));

    // Generate new token for the device
    let token = utils::random_string(TOKEN_LENGTH);

    // Add device
    db.users.create_device(
        &user_id,
        &device_id,
        &token,
        body.initial_device_display_name.clone(),
    )?;

    // Initial data
    db.account_data.update(
        None,
        &user_id,
        &EventType::PushRules,
        serde_json::to_value(ruma::events::push_rules::PushRulesEvent {
            content: ruma::events::push_rules::PushRulesEventContent {
                global: crate::push_rules::default_pushrules(&user_id),
            },
        })
        .expect("data is valid, we just created it")
        .as_object_mut()
        .expect("data is valid, we just created it"),
        &db.globals,
    )?;

    Ok(register::Response {
        access_token: Some(token),
        user_id,
        device_id: Some(device_id),
    }
    .into())
}

#[get("/_matrix/client/r0/login")]
pub fn get_login_route() -> ConduitResult<get_login_types::Response> {
    Ok(get_login_types::Response {
        flows: vec![get_login_types::LoginType::Password],
    }
    .into())
}

#[post("/_matrix/client/r0/login", data = "<body>")]
pub fn login_route(
    db: State<'_, Database>,
    body: Ruma<login::Request>,
) -> ConduitResult<login::Response> {
    // Validate login method
    let user_id =
        // TODO: Other login methods
        if let (login::UserInfo::MatrixId(username), login::LoginInfo::Password { password }) =
            (body.user.clone(), body.login_info.clone())
        {
            let user_id = UserId::parse_with_server_name(username, db.globals.server_name()).map_err(|_| Error::BadRequest(ErrorKind::InvalidUsername, "Username is invalid."))?;
            let hash = db.users.password_hash(&user_id)?.ok_or(Error::BadRequest(ErrorKind::Forbidden, "Wrong username or password."))?;

            let hash_matches =
                argon2::verify_encoded(&hash, password.as_bytes()).unwrap_or(false);

            if !hash_matches {
                return Err(Error::BadRequest(ErrorKind::Forbidden, "Wrong username or password."));
            }

            user_id
        } else {
            return Err(Error::BadRequest(ErrorKind::Forbidden, "Bad login type."));
        };

    // Generate new device id if the user didn't specify one
    let device_id = body
        .body
        .device_id
        .clone()
        .unwrap_or_else(|| utils::random_string(DEVICE_ID_LENGTH));

    // Generate a new token for the device
    let token = utils::random_string(TOKEN_LENGTH);

    // Add device
    db.users.create_device(
        &user_id,
        &device_id,
        &token,
        body.initial_device_display_name.clone(),
    )?;

    Ok(login::Response {
        user_id,
        access_token: token,
        home_server: Some(db.globals.server_name().to_owned()),
        device_id,
        well_known: None,
    }
    .into())
}

#[post("/_matrix/client/r0/logout", data = "<body>")]
pub fn logout_route(
    db: State<'_, Database>,
    body: Ruma<logout::Request>,
) -> ConduitResult<logout::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

    db.users.remove_device(&user_id, &device_id)?;

    Ok(logout::Response.into())
}

#[get("/_matrix/client/r0/capabilities")]
pub fn get_capabilities_route() -> ConduitResult<get_capabilities::Response> {
    let mut available = BTreeMap::new();
    available.insert(
        RoomVersionId::version_5(),
        get_capabilities::RoomVersionStability::Stable,
    );
    available.insert(
        RoomVersionId::version_6(),
        get_capabilities::RoomVersionStability::Stable,
    );

    Ok(get_capabilities::Response {
        capabilities: get_capabilities::Capabilities {
            change_password: None, // None means it is possible
            room_versions: Some(get_capabilities::RoomVersionsCapability {
                default: "6".to_owned(),
                available,
            }),
            custom_capabilities: BTreeMap::new(),
        },
    }
    .into())
}

#[get("/_matrix/client/r0/pushrules", data = "<body>")]
pub fn get_pushrules_all_route(
    db: State<'_, Database>,
    body: Ruma<get_pushrules_all::Request>,
) -> ConduitResult<get_pushrules_all::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    if let EduEvent::PushRules(pushrules) = db
        .account_data
        .get(None, &user_id, &EventType::PushRules)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "PushRules event not found.",
        ))?
        .deserialize()
        .map_err(|_| Error::BadRequest(ErrorKind::NotFound, "PushRules event in db is invalid."))?
    {
        Ok(get_pushrules_all::Response {
            global: pushrules.content.global,
        }
        .into())
    } else {
        Err(Error::bad_database("Pushrules event has wrong content."))
    }
}

#[put(
    "/_matrix/client/r0/pushrules/<_scope>/<_kind>/<_rule_id>",
    //data = "<body>"
)]
pub fn set_pushrule_route(
    //db: State<'_, Database>,
    //body: Ruma<set_pushrule::Request>,
    _scope: String,
    _kind: String,
    _rule_id: String,
) -> ConduitResult<set_pushrule::Response> {
    // TODO
    warn!("TODO: set_pushrule_route");
    Ok(set_pushrule::Response.into())
}

#[put("/_matrix/client/r0/pushrules/<_scope>/<_kind>/<_rule_id>/enabled")]
pub fn set_pushrule_enabled_route(
    _scope: String,
    _kind: String,
    _rule_id: String,
) -> ConduitResult<set_pushrule_enabled::Response> {
    // TODO
    warn!("TODO: set_pushrule_enabled_route");
    Ok(set_pushrule_enabled::Response.into())
}

#[get("/_matrix/client/r0/user/<_user_id>/filter/<_filter_id>")]
pub fn get_filter_route(
    _user_id: String,
    _filter_id: String,
) -> ConduitResult<get_filter::Response> {
    // TODO
    Ok(get_filter::Response {
        filter: filter::FilterDefinition {
            event_fields: None,
            event_format: None,
            account_data: None,
            room: None,
            presence: None,
        },
    }
    .into())
}

#[post("/_matrix/client/r0/user/<_user_id>/filter")]
pub fn create_filter_route(_user_id: String) -> ConduitResult<create_filter::Response> {
    // TODO
    Ok(create_filter::Response {
        filter_id: utils::random_string(10),
    }
    .into())
}

#[put(
    "/_matrix/client/r0/user/<_user_id>/account_data/<_type>",
    data = "<body>"
)]
pub fn set_global_account_data_route(
    db: State<'_, Database>,
    body: Ruma<set_global_account_data::Request>,
    _user_id: String,
    _type: String,
) -> ConduitResult<set_global_account_data::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    db.account_data.update(
        None,
        user_id,
        &EventType::try_from(&body.event_type).expect("EventType::try_from can never fail"),
        json!(
            {"content": serde_json::from_str::<serde_json::Value>(body.data.get())
                .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Data is invalid."))?
            }
        )
        .as_object_mut()
        .expect("we just created a valid object"),
        &db.globals,
    )?;

    Ok(set_global_account_data::Response.into())
}

#[get(
    "/_matrix/client/r0/user/<_user_id>/account_data/<_type>",
    data = "<body>"
)]
pub fn get_global_account_data_route(
    db: State<'_, Database>,
    body: Ruma<get_global_account_data::Request>,
    _user_id: String,
    _type: String,
) -> ConduitResult<get_global_account_data::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let data = db
        .account_data
        .get(
            None,
            user_id,
            &EventType::try_from(&body.event_type).expect("EventType::try_from can never fail"),
        )?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Data not found."))?;

    Ok(get_global_account_data::Response { account_data: data }.into())
}

#[put("/_matrix/client/r0/profile/<_user_id>/displayname", data = "<body>")]
pub fn set_displayname_route(
    db: State<'_, Database>,
    body: Ruma<set_display_name::Request>,
    _user_id: String,
) -> ConduitResult<set_display_name::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    db.users
        .set_displayname(&user_id, body.displayname.clone())?;

    // Send a new membership event into all joined rooms
    for room_id in db.rooms.rooms_joined(&user_id) {
        let room_id = room_id?;
        db.rooms.append_pdu(
            room_id.clone(),
            user_id.clone(),
            EventType::RoomMember,
            serde_json::to_value(ruma::events::room::member::MemberEventContent {
                displayname: body.displayname.clone(),
                ..serde_json::from_value::<EventJson<_>>(
                    db.rooms
                        .room_state_get(&room_id, &EventType::RoomMember, &user_id.to_string())?
                        .ok_or_else(|| {
                            Error::bad_database(
                                "Tried to send displayname update for user not in the room.",
                            )
                        })?
                        .content
                        .clone(),
                )
                .map_err(|_| Error::bad_database("Database contains invalid PDU."))?
                .deserialize()
                .map_err(|_| Error::bad_database("Database contains invalid PDU."))?
            })
            .expect("event is valid, we just created it"),
            None,
            Some(user_id.to_string()),
            None,
            &db.globals,
        )?;
    }

    // Presence update
    db.global_edus.update_presence(
        ruma::events::presence::PresenceEvent {
            content: ruma::events::presence::PresenceEventContent {
                avatar_url: db.users.avatar_url(&user_id)?,
                currently_active: None,
                displayname: db.users.displayname(&user_id)?,
                last_active_ago: Some(
                    utils::millis_since_unix_epoch()
                        .try_into()
                        .expect("time is valid"),
                ),
                presence: ruma::events::presence::PresenceState::Online,
                status_msg: None,
            },
            sender: user_id.clone(),
        },
        &db.globals,
    )?;

    Ok(set_display_name::Response.into())
}

#[get("/_matrix/client/r0/profile/<_user_id>/displayname", data = "<body>")]
pub fn get_displayname_route(
    db: State<'_, Database>,
    body: Ruma<get_display_name::Request>,
    _user_id: String,
) -> ConduitResult<get_display_name::Response> {
    let user_id = body.body.user_id.clone();
    Ok(get_display_name::Response {
        displayname: db.users.displayname(&user_id)?,
    }
    .into())
}

#[put("/_matrix/client/r0/profile/<_user_id>/avatar_url", data = "<body>")]
pub fn set_avatar_url_route(
    db: State<'_, Database>,
    body: Ruma<set_avatar_url::Request>,
    _user_id: String,
) -> ConduitResult<set_avatar_url::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    if let Some(avatar_url) = &body.avatar_url {
        if !avatar_url.starts_with("mxc://") {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "avatar_url has to start with mxc://.",
            ));
        }

        // TODO in the future when we can handle media uploads make sure that this url is our own server
        // TODO also make sure this is valid mxc:// format (not only starting with it)
    }

    db.users.set_avatar_url(&user_id, body.avatar_url.clone())?;

    // Send a new membership event into all joined rooms
    for room_id in db.rooms.rooms_joined(&user_id) {
        let room_id = room_id?;
        db.rooms.append_pdu(
            room_id.clone(),
            user_id.clone(),
            EventType::RoomMember,
            serde_json::to_value(ruma::events::room::member::MemberEventContent {
                avatar_url: body.avatar_url.clone(),
                ..serde_json::from_value::<EventJson<_>>(
                    db.rooms
                        .room_state_get(&room_id, &EventType::RoomMember, &user_id.to_string())?
                        .ok_or_else(|| {
                            Error::bad_database(
                                "Tried to send avatar url update for user not in the room.",
                            )
                        })?
                        .content
                        .clone(),
                )
                .map_err(|_| Error::bad_database("Database contains invalid PDU."))?
                .deserialize()
                .map_err(|_| Error::bad_database("Database contains invalid PDU."))?
            })
            .expect("event is valid, we just created it"),
            None,
            Some(user_id.to_string()),
            None,
            &db.globals,
        )?;
    }

    // Presence update
    db.global_edus.update_presence(
        ruma::events::presence::PresenceEvent {
            content: ruma::events::presence::PresenceEventContent {
                avatar_url: db.users.avatar_url(&user_id)?,
                currently_active: None,
                displayname: db.users.displayname(&user_id)?,
                last_active_ago: Some(
                    utils::millis_since_unix_epoch()
                        .try_into()
                        .expect("time is valid"),
                ),
                presence: ruma::events::presence::PresenceState::Online,
                status_msg: None,
            },
            sender: user_id.clone(),
        },
        &db.globals,
    )?;

    Ok(set_avatar_url::Response.into())
}

#[get("/_matrix/client/r0/profile/<_user_id>/avatar_url", data = "<body>")]
pub fn get_avatar_url_route(
    db: State<'_, Database>,
    body: Ruma<get_avatar_url::Request>,
    _user_id: String,
) -> ConduitResult<get_avatar_url::Response> {
    let user_id = body.body.user_id.clone();
    Ok(get_avatar_url::Response {
        avatar_url: db.users.avatar_url(&user_id)?,
    }
    .into())
}

#[get("/_matrix/client/r0/profile/<_user_id>", data = "<body>")]
pub fn get_profile_route(
    db: State<'_, Database>,
    body: Ruma<get_profile::Request>,
    _user_id: String,
) -> ConduitResult<get_profile::Response> {
    let user_id = body.body.user_id.clone();
    let avatar_url = db.users.avatar_url(&user_id)?;
    let displayname = db.users.displayname(&user_id)?;

    if avatar_url.is_none() && displayname.is_none() {
        // Return 404 if we don't have a profile for this id
        return Err(Error::BadRequest(
            ErrorKind::NotFound,
            "Profile was not found.",
        ));
    }

    Ok(get_profile::Response {
        avatar_url,
        displayname,
    }
    .into())
}

#[put("/_matrix/client/r0/presence/<_user_id>/status", data = "<body>")]
pub fn set_presence_route(
    db: State<'_, Database>,
    body: Ruma<set_presence::Request>,
    _user_id: String,
) -> ConduitResult<set_presence::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    db.global_edus.update_presence(
        ruma::events::presence::PresenceEvent {
            content: ruma::events::presence::PresenceEventContent {
                avatar_url: db.users.avatar_url(&user_id)?,
                currently_active: None,
                displayname: db.users.displayname(&user_id)?,
                last_active_ago: Some(
                    utils::millis_since_unix_epoch()
                        .try_into()
                        .expect("time is valid"),
                ),
                presence: body.presence,
                status_msg: body.status_msg.clone(),
            },
            sender: user_id.clone(),
        },
        &db.globals,
    )?;

    Ok(set_presence::Response.into())
}

#[post("/_matrix/client/r0/keys/upload", data = "<body>")]
pub fn upload_keys_route(
    db: State<'_, Database>,
    body: Ruma<upload_keys::Request>,
) -> ConduitResult<upload_keys::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

    if let Some(one_time_keys) = &body.one_time_keys {
        for (key_key, key_value) in one_time_keys {
            db.users
                .add_one_time_key(user_id, device_id, key_key, key_value)?;
        }
    }

    if let Some(device_keys) = &body.device_keys {
        // This check is needed to assure that signatures are kept
        if db.users.get_device_keys(user_id, device_id)?.is_none() {
            db.users
                .add_device_keys(user_id, device_id, device_keys, &db.globals)?;
        }
    }

    Ok(upload_keys::Response {
        one_time_key_counts: db.users.count_one_time_keys(user_id, device_id)?,
    }
    .into())
}

#[post("/_matrix/client/r0/keys/query", data = "<body>")]
pub fn get_keys_route(
    db: State<'_, Database>,
    body: Ruma<get_keys::Request>,
) -> ConduitResult<get_keys::Response> {
    let sender_id = body.user_id.as_ref().expect("user is authenticated");

    let mut master_keys = BTreeMap::new();
    let mut self_signing_keys = BTreeMap::new();
    let mut user_signing_keys = BTreeMap::new();
    let mut device_keys = BTreeMap::new();

    for (user_id, device_ids) in &body.device_keys {
        if device_ids.is_empty() {
            let mut container = BTreeMap::new();
            for device_id in db.users.all_device_ids(user_id) {
                let device_id = device_id?;
                if let Some(mut keys) = db.users.get_device_keys(user_id, &device_id)? {
                    let metadata = db
                        .users
                        .get_device_metadata(user_id, &device_id)?
                        .ok_or_else(|| {
                            Error::bad_database("all_device_keys contained nonexistent device.")
                        })?;

                    keys.unsigned = Some(keys::UnsignedDeviceInfo {
                        device_display_name: metadata.display_name,
                    });

                    container.insert(device_id.to_owned(), keys);
                }
            }
            device_keys.insert(user_id.clone(), container);
        } else {
            for device_id in device_ids {
                let mut container = BTreeMap::new();
                if let Some(mut keys) = db.users.get_device_keys(&user_id.clone(), &device_id)? {
                    let metadata = db.users.get_device_metadata(user_id, &device_id)?.ok_or(
                        Error::BadRequest(
                            ErrorKind::InvalidParam,
                            "Tried to get keys for nonexistent device.",
                        ),
                    )?;

                    keys.unsigned = Some(keys::UnsignedDeviceInfo {
                        device_display_name: metadata.display_name,
                    });

                    container.insert(device_id.clone(), keys);
                }
                device_keys.insert(user_id.clone(), container);
            }
        }

        if let Some(master_key) = db.users.get_master_key(user_id, sender_id)? {
            master_keys.insert(user_id.clone(), master_key);
        }
        if let Some(self_signing_key) = db.users.get_self_signing_key(user_id, sender_id)? {
            self_signing_keys.insert(user_id.clone(), self_signing_key);
        }
        if user_id == sender_id {
            if let Some(user_signing_key) = db.users.get_user_signing_key(sender_id)? {
                user_signing_keys.insert(user_id.clone(), user_signing_key);
            }
        }
    }

    Ok(get_keys::Response {
        master_keys,
        self_signing_keys,
        user_signing_keys,
        device_keys,
        failures: BTreeMap::new(),
    }
    .into())
}

#[post("/_matrix/client/r0/keys/claim", data = "<body>")]
pub fn claim_keys_route(
    db: State<'_, Database>,
    body: Ruma<claim_keys::Request>,
) -> ConduitResult<claim_keys::Response> {
    let mut one_time_keys = BTreeMap::new();
    for (user_id, map) in &body.one_time_keys {
        let mut container = BTreeMap::new();
        for (device_id, key_algorithm) in map {
            if let Some(one_time_keys) =
                db.users
                    .take_one_time_key(user_id, device_id, key_algorithm)?
            {
                let mut c = BTreeMap::new();
                c.insert(one_time_keys.0, one_time_keys.1);
                container.insert(device_id.clone(), c);
            }
        }
        one_time_keys.insert(user_id.clone(), container);
    }

    Ok(claim_keys::Response {
        failures: BTreeMap::new(),
        one_time_keys,
    }
    .into())
}

#[post("/_matrix/client/unstable/room_keys/version", data = "<body>")]
pub fn create_backup_route(
    db: State<'_, Database>,
    body: Ruma<create_backup::Request>,
) -> ConduitResult<create_backup::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");
    let version = db
        .key_backups
        .create_backup(&user_id, &body.algorithm, &db.globals)?;

    Ok(create_backup::Response { version }.into())
}

#[put(
    "/_matrix/client/unstable/room_keys/version/<_version>",
    data = "<body>"
)]
pub fn update_backup_route(
    db: State<'_, Database>,
    body: Ruma<update_backup::Request>,
    _version: String,
) -> ConduitResult<update_backup::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");
    db.key_backups
        .update_backup(&user_id, &body.version, &body.algorithm, &db.globals)?;

    Ok(update_backup::Response.into())
}

#[get("/_matrix/client/unstable/room_keys/version", data = "<body>")]
pub fn get_latest_backup_route(
    db: State<'_, Database>,
    body: Ruma<get_latest_backup::Request>,
) -> ConduitResult<get_latest_backup::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let (version, algorithm) =
        db.key_backups
            .get_latest_backup(&user_id)?
            .ok_or(Error::BadRequest(
                ErrorKind::NotFound,
                "Key backup does not exist.",
            ))?;

    Ok(get_latest_backup::Response {
        algorithm,
        count: (db.key_backups.count_keys(user_id, &version)? as u32).into(),
        etag: db.key_backups.get_etag(user_id, &version)?,
        version,
    }
    .into())
}

#[get(
    "/_matrix/client/unstable/room_keys/version/<_version>",
    data = "<body>"
)]
pub fn get_backup_route(
    db: State<'_, Database>,
    body: Ruma<get_backup::Request>,
    _version: String,
) -> ConduitResult<get_backup::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");
    let algorithm =
        db.key_backups
            .get_backup(&user_id, &body.version)?
            .ok_or(Error::BadRequest(
                ErrorKind::NotFound,
                "Key backup does not exist.",
            ))?;

    Ok(get_backup::Response {
        algorithm,
        count: (db.key_backups.count_keys(user_id, &body.version)? as u32).into(),
        etag: db.key_backups.get_etag(user_id, &body.version)?,
        version: body.version.clone(),
    }
    .into())
}

#[put("/_matrix/client/unstable/room_keys/keys", data = "<body>")]
pub fn add_backup_keys_route(
    db: State<'_, Database>,
    body: Ruma<add_backup_keys::Request>,
) -> ConduitResult<add_backup_keys::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    for (room_id, room) in &body.rooms {
        for (session_id, key_data) in &room.sessions {
            db.key_backups.add_key(
                &user_id,
                &body.version,
                &room_id,
                &session_id,
                &key_data,
                &db.globals,
            )?
        }
    }

    Ok(add_backup_keys::Response {
        count: (db.key_backups.count_keys(user_id, &body.version)? as u32).into(),
        etag: db.key_backups.get_etag(user_id, &body.version)?,
    }
    .into())
}

#[get("/_matrix/client/unstable/room_keys/keys", data = "<body>")]
pub fn get_backup_keys_route(
    db: State<'_, Database>,
    body: Ruma<get_backup_keys::Request>,
) -> ConduitResult<get_backup_keys::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let rooms = db.key_backups.get_all(&user_id, &body.version)?;

    Ok(get_backup_keys::Response { rooms }.into())
}

#[post("/_matrix/client/r0/rooms/<_room_id>/read_markers", data = "<body>")]
pub fn set_read_marker_route(
    db: State<'_, Database>,
    body: Ruma<set_read_marker::Request>,
    _room_id: String,
) -> ConduitResult<set_read_marker::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    db.account_data.update(
        Some(&body.room_id),
        &user_id,
        &EventType::FullyRead,
        serde_json::to_value(ruma::events::fully_read::FullyReadEvent {
            content: ruma::events::fully_read::FullyReadEventContent {
                event_id: body.fully_read.clone(),
            },
            room_id: Some(body.room_id.clone()),
        })
        .expect("we just created a valid event")
        .as_object_mut()
        .expect("we just created a valid event"),
        &db.globals,
    )?;

    if let Some(event) = &body.read_receipt {
        db.rooms.edus.room_read_set(
            &body.room_id,
            &user_id,
            db.rooms.get_pdu_count(event)?.ok_or(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Event does not exist.",
            ))?,
        )?;

        let mut user_receipts = BTreeMap::new();
        user_receipts.insert(
            user_id.clone(),
            ruma::events::receipt::Receipt {
                ts: Some(SystemTime::now()),
            },
        );
        let mut receipt_content = BTreeMap::new();
        receipt_content.insert(
            event.clone(),
            ruma::events::receipt::Receipts {
                read: Some(user_receipts),
            },
        );

        db.rooms.edus.roomlatest_update(
            &user_id,
            &body.room_id,
            EduEvent::Receipt(ruma::events::receipt::ReceiptEvent {
                content: receipt_content,
                room_id: None, // None because it can be inferred
            }),
            &db.globals,
        )?;
    }
    Ok(set_read_marker::Response.into())
}

#[put(
    "/_matrix/client/r0/rooms/<_room_id>/typing/<_user_id>",
    data = "<body>"
)]
pub fn create_typing_event_route(
    db: State<'_, Database>,
    body: Ruma<create_typing_event::Request>,
    _room_id: String,
    _user_id: String,
) -> ConduitResult<create_typing_event::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    if body.typing {
        db.rooms.edus.roomactive_add(
            &user_id,
            &body.room_id,
            body.timeout.map(|d| d.as_millis() as u64).unwrap_or(30000)
                + utils::millis_since_unix_epoch(),
            &db.globals,
        )?;
    } else {
        db.rooms
            .edus
            .roomactive_remove(&user_id, &body.room_id, &db.globals)?;
    }

    Ok(create_typing_event::Response.into())
}

#[post("/_matrix/client/r0/createRoom", data = "<body>")]
pub fn create_room_route(
    db: State<'_, Database>,
    body: Ruma<create_room::Request>,
) -> ConduitResult<create_room::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let room_id = RoomId::new(db.globals.server_name())
        .map_err(|_| Error::bad_database("Server name is invalid."))?;

    let alias = body
        .room_alias_name
        .as_ref()
        .map_or(Ok(None), |localpart| {
            // TODO: Check for invalid characters and maximum length
            let alias =
                RoomAliasId::try_from(format!("#{}:{}", localpart, db.globals.server_name()))
                    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid alias."))?;

            if db.rooms.id_from_alias(&alias)?.is_some() {
                Err(Error::BadRequest(
                    ErrorKind::RoomInUse,
                    "Room alias already exists.",
                ))
            } else {
                Ok(Some(alias))
            }
        })?;

    // 1. The room create event
    db.rooms.append_pdu(
        room_id.clone(),
        user_id.clone(),
        EventType::RoomCreate,
        serde_json::to_value(ruma::events::room::create::CreateEventContent {
            creator: user_id.clone(),
            federate: body.creation_content.as_ref().map_or(true, |c| c.federate),
            predecessor: body
                .creation_content
                .as_ref()
                .and_then(|c| c.predecessor.clone()),
            room_version: RoomVersionId::version_6(),
        })
        .expect("event is valid, we just created it"),
        None,
        Some("".to_owned()),
        None,
        &db.globals,
    )?;

    // 2. Let the room creator join
    db.rooms.append_pdu(
        room_id.clone(),
        user_id.clone(),
        EventType::RoomMember,
        serde_json::to_value(member::MemberEventContent {
            membership: member::MembershipState::Join,
            displayname: db.users.displayname(&user_id)?,
            avatar_url: db.users.avatar_url(&user_id)?,
            is_direct: body.is_direct,
            third_party_invite: None,
        })
        .expect("event is valid, we just created it"),
        None,
        Some(user_id.to_string()),
        None,
        &db.globals,
    )?;

    // Figure out preset. We need it for power levels and preset specific events
    let visibility = body.visibility.unwrap_or(room::Visibility::Private);
    let preset = body.preset.unwrap_or_else(|| match visibility {
        room::Visibility::Private => create_room::RoomPreset::PrivateChat,
        room::Visibility::Public => create_room::RoomPreset::PublicChat,
    });

    // 3. Power levels
    let mut users = BTreeMap::new();
    users.insert(user_id.clone(), 100.into());
    for invite_user_id in &body.invite {
        users.insert(invite_user_id.clone(), 100.into());
    }

    let power_levels_content = if let Some(power_levels) = &body.power_level_content_override {
        serde_json::from_str(power_levels.json().get()).map_err(|_| {
            Error::BadRequest(ErrorKind::BadJson, "Invalid power_level_content_override.")
        })?
    } else {
        serde_json::to_value(ruma::events::room::power_levels::PowerLevelsEventContent {
            ban: 50.into(),
            events: BTreeMap::new(),
            events_default: 0.into(),
            invite: 50.into(),
            kick: 50.into(),
            redact: 50.into(),
            state_default: 50.into(),
            users,
            users_default: 0.into(),
            notifications: ruma::events::room::power_levels::NotificationPowerLevels {
                room: 50.into(),
            },
        })
        .expect("event is valid, we just created it")
    };
    db.rooms.append_pdu(
        room_id.clone(),
        user_id.clone(),
        EventType::RoomPowerLevels,
        power_levels_content,
        None,
        Some("".to_owned()),
        None,
        &db.globals,
    )?;

    // 4. Events set by preset
    // 4.1 Join Rules
    db.rooms.append_pdu(
        room_id.clone(),
        user_id.clone(),
        EventType::RoomJoinRules,
        match preset {
            create_room::RoomPreset::PublicChat => {
                serde_json::to_value(join_rules::JoinRulesEventContent {
                    join_rule: join_rules::JoinRule::Public,
                })
                .expect("event is valid, we just created it")
            }
            _ => serde_json::to_value(join_rules::JoinRulesEventContent {
                join_rule: join_rules::JoinRule::Invite,
            })
            .expect("event is valid, we just created it"),
        },
        None,
        Some("".to_owned()),
        None,
        &db.globals,
    )?;

    // 4.2 History Visibility
    db.rooms.append_pdu(
        room_id.clone(),
        user_id.clone(),
        EventType::RoomHistoryVisibility,
        serde_json::to_value(history_visibility::HistoryVisibilityEventContent {
            history_visibility: history_visibility::HistoryVisibility::Shared,
        })
        .expect("event is valid, we just created it"),
        None,
        Some("".to_owned()),
        None,
        &db.globals,
    )?;

    // 4.3 Guest Access
    db.rooms.append_pdu(
        room_id.clone(),
        user_id.clone(),
        EventType::RoomGuestAccess,
        match preset {
            create_room::RoomPreset::PublicChat => {
                serde_json::to_value(guest_access::GuestAccessEventContent {
                    guest_access: guest_access::GuestAccess::Forbidden,
                })
                .expect("event is valid, we just created it")
            }
            _ => serde_json::to_value(guest_access::GuestAccessEventContent {
                guest_access: guest_access::GuestAccess::CanJoin,
            })
            .expect("event is valid, we just created it"),
        },
        None,
        Some("".to_owned()),
        None,
        &db.globals,
    )?;

    // 5. Events listed in initial_state
    for create_room::InitialStateEvent {
        event_type,
        state_key,
        content,
    } in &body.initial_state
    {
        db.rooms.append_pdu(
            room_id.clone(),
            user_id.clone(),
            event_type.clone(),
            serde_json::from_str(content.get()).map_err(|_| {
                Error::BadRequest(ErrorKind::BadJson, "Invalid initial_state content.")
            })?,
            None,
            state_key.clone(),
            None,
            &db.globals,
        )?;
    }

    // 6. Events implied by name and topic
    if let Some(name) = &body.name {
        db.rooms.append_pdu(
            room_id.clone(),
            user_id.clone(),
            EventType::RoomName,
            serde_json::to_value(
                name::NameEventContent::new(name.clone())
                    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Name is invalid."))?,
            )
            .expect("event is valid, we just created it"),
            None,
            Some("".to_owned()),
            None,
            &db.globals,
        )?;
    }

    if let Some(topic) = &body.topic {
        db.rooms.append_pdu(
            room_id.clone(),
            user_id.clone(),
            EventType::RoomTopic,
            serde_json::to_value(topic::TopicEventContent {
                topic: topic.clone(),
            })
            .expect("event is valid, we just created it"),
            None,
            Some("".to_owned()),
            None,
            &db.globals,
        )?;
    }

    // 7. Events implied by invite (and TODO: invite_3pid)
    for user in &body.invite {
        db.rooms.append_pdu(
            room_id.clone(),
            user_id.clone(),
            EventType::RoomMember,
            serde_json::to_value(member::MemberEventContent {
                membership: member::MembershipState::Invite,
                displayname: db.users.displayname(&user)?,
                avatar_url: db.users.avatar_url(&user)?,
                is_direct: body.is_direct,
                third_party_invite: None,
            })
            .expect("event is valid, we just created it"),
            None,
            Some(user.to_string()),
            None,
            &db.globals,
        )?;
    }

    // Homeserver specific stuff
    if let Some(alias) = alias {
        db.rooms.set_alias(&alias, Some(&room_id), &db.globals)?;
    }

    if let Some(room::Visibility::Public) = body.visibility {
        db.rooms.set_public(&room_id, true)?;
    }

    Ok(create_room::Response { room_id }.into())
}

#[put(
    "/_matrix/client/r0/rooms/<_room_id>/redact/<_event_id>/<_txn_id>",
    data = "<body>"
)]
pub fn redact_event_route(
    db: State<'_, Database>,
    body: Ruma<redact_event::Request>,
    _room_id: String,
    _event_id: String,
    _txn_id: String,
) -> ConduitResult<redact_event::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let event_id = db.rooms.append_pdu(
        body.room_id.clone(),
        user_id.clone(),
        EventType::RoomRedaction,
        serde_json::to_value(redaction::RedactionEventContent {
            reason: body.reason.clone(),
        })
        .expect("event is valid, we just created it"),
        None,
        None,
        Some(body.event_id.clone()),
        &db.globals,
    )?;

    Ok(redact_event::Response { event_id }.into())
}

#[put("/_matrix/client/r0/directory/room/<_room_alias>", data = "<body>")]
pub fn create_alias_route(
    db: State<'_, Database>,
    body: Ruma<create_alias::Request>,
    _room_alias: String,
) -> ConduitResult<create_alias::Response> {
    if db.rooms.id_from_alias(&body.room_alias)?.is_some() {
        return Err(Error::Conflict("Alias already exists."));
    }

    db.rooms
        .set_alias(&body.room_alias, Some(&body.room_id), &db.globals)?;

    Ok(create_alias::Response.into())
}

#[delete("/_matrix/client/r0/directory/room/<_room_alias>", data = "<body>")]
pub fn delete_alias_route(
    db: State<'_, Database>,
    body: Ruma<delete_alias::Request>,
    _room_alias: String,
) -> ConduitResult<delete_alias::Response> {
    db.rooms.set_alias(&body.room_alias, None, &db.globals)?;

    Ok(delete_alias::Response.into())
}

#[get("/_matrix/client/r0/directory/room/<_room_alias>", data = "<body>")]
pub fn get_alias_route(
    db: State<'_, Database>,
    body: Ruma<get_alias::Request>,
    _room_alias: String,
) -> ConduitResult<get_alias::Response> {
    if body.room_alias.server_name() != db.globals.server_name() {
        todo!("ask remote server");
    }

    let room_id = db
        .rooms
        .id_from_alias(&body.room_alias)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Room with alias not found.",
        ))?;

    Ok(get_alias::Response {
        room_id,
        servers: vec![db.globals.server_name().to_owned()],
    }
    .into())
}

#[post("/_matrix/client/r0/rooms/<_room_id>/join", data = "<body>")]
pub fn join_room_by_id_route(
    db: State<'_, Database>,
    body: Ruma<join_room_by_id::Request>,
    _room_id: String,
) -> ConduitResult<join_room_by_id::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    // TODO: Ask a remote server if we don't have this room

    let event = member::MemberEventContent {
        membership: member::MembershipState::Join,
        displayname: db.users.displayname(&user_id)?,
        avatar_url: db.users.avatar_url(&user_id)?,
        is_direct: None,
        third_party_invite: None,
    };

    db.rooms.append_pdu(
        body.room_id.clone(),
        user_id.clone(),
        EventType::RoomMember,
        serde_json::to_value(event).expect("event is valid, we just created it"),
        None,
        Some(user_id.to_string()),
        None,
        &db.globals,
    )?;

    Ok(join_room_by_id::Response {
        room_id: body.room_id.clone(),
    }
    .into())
}

#[post("/_matrix/client/r0/join/<_room_id_or_alias>", data = "<body>")]
pub fn join_room_by_id_or_alias_route(
    db: State<'_, Database>,
    body: Ruma<join_room_by_id_or_alias::Request>,
    _room_id_or_alias: String,
) -> ConduitResult<join_room_by_id_or_alias::Response> {
    let room_id = RoomId::try_from(body.room_id_or_alias.clone()).or_else(|alias| {
        Ok::<_, Error>(db.rooms.id_from_alias(&alias)?.ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Room not found (TODO: Federation).",
        ))?)
    })?;

    let body = Ruma {
        user_id: body.user_id.clone(),
        device_id: body.device_id.clone(),
        json_body: None,
        body: join_room_by_id::Request {
            room_id,
            third_party_signed: body.third_party_signed.clone(),
        },
    };

    Ok(join_room_by_id_or_alias::Response {
        room_id: join_room_by_id_route(db, body, "".to_owned())?.0.room_id,
    }
    .into())
}

#[post("/_matrix/client/r0/rooms/<_room_id>/leave", data = "<body>")]
pub fn leave_room_route(
    db: State<'_, Database>,
    body: Ruma<leave_room::Request>,
    _room_id: String,
) -> ConduitResult<leave_room::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let mut event = serde_json::from_value::<EventJson<member::MemberEventContent>>(
        db.rooms
            .room_state_get(&body.room_id, &EventType::RoomMember, &user_id.to_string())?
            .ok_or(Error::BadRequest(
                ErrorKind::BadState,
                "Cannot leave a room you are not a member of.",
            ))?
            .content
            .clone(),
    )
    .map_err(|_| Error::bad_database("Invalid member event in database."))?
    .deserialize()
    .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = member::MembershipState::Leave;

    db.rooms.append_pdu(
        body.room_id.clone(),
        user_id.clone(),
        EventType::RoomMember,
        serde_json::to_value(event).expect("event is valid, we just created it"),
        None,
        Some(user_id.to_string()),
        None,
        &db.globals,
    )?;

    Ok(leave_room::Response.into())
}

#[post("/_matrix/client/r0/rooms/<_room_id>/kick", data = "<body>")]
pub fn kick_user_route(
    db: State<'_, Database>,
    body: Ruma<kick_user::Request>,
    _room_id: String,
) -> ConduitResult<kick_user::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let mut event =
        serde_json::from_value::<EventJson<ruma::events::room::member::MemberEventContent>>(
            db.rooms
                .room_state_get(&body.room_id, &EventType::RoomMember, &user_id.to_string())?
                .ok_or(Error::BadRequest(
                    ErrorKind::BadState,
                    "Cannot kick member that's not in the room.",
                ))?
                .content
                .clone(),
        )
        .map_err(|_| Error::bad_database("Invalid member event in database."))?
        .deserialize()
        .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = ruma::events::room::member::MembershipState::Leave;
    // TODO: reason

    db.rooms.append_pdu(
        body.room_id.clone(),
        user_id.clone(), // Sender
        EventType::RoomMember,
        serde_json::to_value(event).expect("event is valid, we just created it"),
        None,
        Some(body.body.user_id.to_string()),
        None,
        &db.globals,
    )?;

    Ok(kick_user::Response.into())
}

#[post("/_matrix/client/r0/rooms/<_room_id>/ban", data = "<body>")]
pub fn ban_user_route(
    db: State<'_, Database>,
    body: Ruma<ban_user::Request>,
    _room_id: String,
) -> ConduitResult<ban_user::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    // TODO: reason

    let event = db
        .rooms
        .room_state_get(&body.room_id, &EventType::RoomMember, &user_id.to_string())?
        .map_or(
            Ok::<_, Error>(member::MemberEventContent {
                membership: member::MembershipState::Ban,
                displayname: db.users.displayname(&user_id)?,
                avatar_url: db.users.avatar_url(&user_id)?,
                is_direct: None,
                third_party_invite: None,
            }),
            |event| {
                let mut event = serde_json::from_value::<EventJson<member::MemberEventContent>>(
                    event.content.clone(),
                )
                .map_err(|_| Error::bad_database("Invalid member event in database."))?
                .deserialize()
                .map_err(|_| Error::bad_database("Invalid member event in database."))?;
                event.membership = ruma::events::room::member::MembershipState::Ban;
                Ok(event)
            },
        )?;

    db.rooms.append_pdu(
        body.room_id.clone(),
        user_id.clone(), // Sender
        EventType::RoomMember,
        serde_json::to_value(event).expect("event is valid, we just created it"),
        None,
        Some(body.body.user_id.to_string()),
        None,
        &db.globals,
    )?;

    Ok(ban_user::Response.into())
}

#[post("/_matrix/client/r0/rooms/<_room_id>/unban", data = "<body>")]
pub fn unban_user_route(
    db: State<'_, Database>,
    body: Ruma<unban_user::Request>,
    _room_id: String,
) -> ConduitResult<unban_user::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let mut event =
        serde_json::from_value::<EventJson<ruma::events::room::member::MemberEventContent>>(
            db.rooms
                .room_state_get(&body.room_id, &EventType::RoomMember, &user_id.to_string())?
                .ok_or(Error::BadRequest(
                    ErrorKind::BadState,
                    "Cannot unban a user who is not banned.",
                ))?
                .content
                .clone(),
        )
        .map_err(|_| Error::bad_database("Invalid member event in database."))?
        .deserialize()
        .map_err(|_| Error::bad_database("Invalid member event in database."))?;

    event.membership = ruma::events::room::member::MembershipState::Leave;

    db.rooms.append_pdu(
        body.room_id.clone(),
        user_id.clone(), // Sender
        EventType::RoomMember,
        serde_json::to_value(event).expect("event is valid, we just created it"),
        None,
        Some(body.body.user_id.to_string()),
        None,
        &db.globals,
    )?;

    Ok(unban_user::Response.into())
}

#[post("/_matrix/client/r0/rooms/<_room_id>/forget", data = "<body>")]
pub fn forget_room_route(
    db: State<'_, Database>,
    body: Ruma<forget_room::Request>,
    _room_id: String,
) -> ConduitResult<forget_room::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    db.rooms.forget(&body.room_id, &user_id)?;

    Ok(forget_room::Response.into())
}

#[post("/_matrix/client/r0/rooms/<_room_id>/invite", data = "<body>")]
pub fn invite_user_route(
    db: State<'_, Database>,
    body: Ruma<invite_user::Request>,
    _room_id: String,
) -> ConduitResult<invite_user::Response> {
    if let invite_user::InvitationRecipient::UserId { user_id } = &body.recipient {
        db.rooms.append_pdu(
            body.room_id.clone(),
            body.user_id.clone().expect("user is authenticated"),
            EventType::RoomMember,
            serde_json::to_value(member::MemberEventContent {
                membership: member::MembershipState::Invite,
                displayname: db.users.displayname(&user_id)?,
                avatar_url: db.users.avatar_url(&user_id)?,
                is_direct: None,
                third_party_invite: None,
            })
            .expect("event is valid, we just created it"),
            None,
            Some(user_id.to_string()),
            None,
            &db.globals,
        )?;

        Ok(invite_user::Response.into())
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "User not found."))
    }
}

#[put("/_matrix/client/r0/directory/list/room/<_room_id>", data = "<body>")]
pub async fn set_room_visibility_route(
    db: State<'_, Database>,
    body: Ruma<set_room_visibility::Request>,
    _room_id: String,
) -> ConduitResult<set_room_visibility::Response> {
    match body.visibility {
        room::Visibility::Public => db.rooms.set_public(&body.room_id, true)?,
        room::Visibility::Private => db.rooms.set_public(&body.room_id, false)?,
    }

    Ok(set_room_visibility::Response.into())
}

#[get("/_matrix/client/r0/directory/list/room/<_room_id>", data = "<body>")]
pub async fn get_room_visibility_route(
    db: State<'_, Database>,
    body: Ruma<get_room_visibility::Request>,
    _room_id: String,
) -> ConduitResult<get_room_visibility::Response> {
    Ok(get_room_visibility::Response {
        visibility: if db.rooms.is_public_room(&body.room_id)? {
            room::Visibility::Public
        } else {
            room::Visibility::Private
        },
    }
    .into())
}

#[get("/_matrix/client/r0/publicRooms", data = "<body>")]
pub async fn get_public_rooms_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms::Request>,
) -> ConduitResult<get_public_rooms::Response> {
    let Ruma {
        body:
            get_public_rooms::Request {
                limit,
                server,
                since,
            },
        user_id,
        device_id,
        json_body,
    } = body;

    let get_public_rooms_filtered::Response {
        chunk,
        prev_batch,
        next_batch,
        total_room_count_estimate,
    } = get_public_rooms_filtered_route(
        db,
        Ruma {
            body: get_public_rooms_filtered::Request {
                filter: None,
                limit,
                room_network: get_public_rooms_filtered::RoomNetwork::Matrix,
                server,
                since,
            },
            user_id,
            device_id,
            json_body,
        },
    )
    .await?
    .0;

    Ok(get_public_rooms::Response {
        chunk,
        prev_batch,
        next_batch,
        total_room_count_estimate,
    }
    .into())
}

#[post("/_matrix/client/r0/publicRooms", data = "<body>")]
pub async fn get_public_rooms_filtered_route(
    db: State<'_, Database>,
    body: Ruma<get_public_rooms_filtered::Request>,
) -> ConduitResult<get_public_rooms_filtered::Response> {
    let mut chunk = db
        .rooms
        .public_rooms()
        .map(|room_id| {
            let room_id = room_id?;

            // TODO: Do not load full state?
            let state = db.rooms.room_state_full(&room_id)?;

            let chunk = directory::PublicRoomsChunk {
                aliases: Vec::new(),
                canonical_alias: state.get(&(EventType::RoomCanonicalAlias, "".to_owned())).map_or(Ok::<_, Error>(None), |s| {
                    Ok(serde_json::from_value::<
                            EventJson<ruma::events::room::canonical_alias::CanonicalAliasEventContent>,
                        >(s.content.clone())
                        .map_err(|_| Error::bad_database("Invalid canonical alias event in database."))?
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid canonical alias event in database."))?
                        .alias)
                })?,
                name: state.get(&(EventType::RoomName, "".to_owned())).map_or(Ok::<_, Error>(None), |s| {
                    Ok(serde_json::from_value::<EventJson<ruma::events::room::name::NameEventContent>>(
                        s.content.clone(),
                    )
                    .map_err(|_| Error::bad_database("Invalid room name event in database."))?
                    .deserialize()
                    .map_err(|_| Error::bad_database("Invalid room name event in database."))?
                    .name()
                    .map(|n| n.to_owned()))
                })?,
                num_joined_members: (db.rooms.room_members(&room_id).count() as u32).into(),
                room_id,
                topic: state.get(&(EventType::RoomTopic, "".to_owned())).map_or(Ok::<_, Error>(None), |s| {
                    Ok(Some(serde_json::from_value::<
                            EventJson<ruma::events::room::topic::TopicEventContent>,
                        >(s.content.clone())
                        .map_err(|_| Error::bad_database("Invalid room topic event in database."))?
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid room topic event in database."))?
                        .topic))
                })?,
                world_readable: state.get(&(EventType::RoomHistoryVisibility, "".to_owned())).map_or(Ok::<_, Error>(false), |s| {
                    Ok(serde_json::from_value::<
                            EventJson<ruma::events::room::history_visibility::HistoryVisibilityEventContent>,
                        >(s.content.clone())
                        .map_err(|_| Error::bad_database("Invalid room history visibility event in database."))?
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid room history visibility event in database."))?
                        .history_visibility == history_visibility::HistoryVisibility::WorldReadable)
                })?,
                guest_can_join: state.get(&(EventType::RoomGuestAccess, "".to_owned())).map_or(Ok::<_, Error>(false), |s| {
                    Ok(serde_json::from_value::<
                            EventJson<ruma::events::room::guest_access::GuestAccessEventContent>,
                        >(s.content.clone())
                        .map_err(|_| Error::bad_database("Invalid room guest access event in database."))?
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid room guest access event in database."))?
                        .guest_access == guest_access::GuestAccess::CanJoin)
                })?,
                avatar_url: state.get(&(EventType::RoomAvatar, "".to_owned())).map_or( Ok::<_, Error>(None),|s| {
                    Ok(Some(serde_json::from_value::<
                            EventJson<ruma::events::room::avatar::AvatarEventContent>,
                        >(s.content.clone())
                        .map_err(|_| Error::bad_database("Invalid room avatar event in database."))?
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid room avatar event in database."))?
                        .url))
                })?,
            };
            Ok::<_, Error>(chunk)
        })
        .filter_map(|r| r.ok()) // Filter out buggy rooms
        .collect::<Vec<_>>();

    chunk.sort_by(|l, r| r.num_joined_members.cmp(&l.num_joined_members));

    /*
    chunk.extend_from_slice(
        &server_server::send_request(
            &db,
            "privacytools.io".to_owned(),
            ruma::api::federation::v1::get_public_rooms::Request {
                limit: Some(20_u32.into()),
                since: None,
                room_network: ruma::api::federation::v1::get_public_rooms::RoomNetwork::Matrix,
            },
        )
        .await
        ?
        .chunk
        .into_iter()
        .map(|c| serde_json::from_str(&serde_json::to_string(&c)?)?)
        .collect::<Vec<_>>(),
    );
    */

    let total_room_count_estimate = (chunk.len() as u32).into();

    Ok(get_public_rooms_filtered::Response {
        chunk,
        prev_batch: None,
        next_batch: None,
        total_room_count_estimate: Some(total_room_count_estimate),
    }
    .into())
}

#[post("/_matrix/client/r0/user_directory/search", data = "<body>")]
pub fn search_users_route(
    db: State<'_, Database>,
    body: Ruma<search_users::Request>,
) -> ConduitResult<search_users::Response> {
    Ok(search_users::Response {
        results: db
            .users
            .iter()
            .filter_map(|user_id| {
                // Filter out buggy users (they should not exist, but you never know...)
                let user_id = user_id.ok()?;
                Some(search_users::User {
                    user_id: user_id.clone(),
                    display_name: db.users.displayname(&user_id).ok()?,
                    avatar_url: db.users.avatar_url(&user_id).ok()?,
                })
            })
            .filter(|user| {
                user.user_id.to_string().contains(&body.search_term)
                    || user
                        .display_name
                        .as_ref()
                        .filter(|name| name.contains(&body.search_term))
                        .is_some()
            })
            .collect(),
        limited: false,
    }
    .into())
}

#[get("/_matrix/client/r0/rooms/<_room_id>/members", data = "<body>")]
pub fn get_member_events_route(
    db: State<'_, Database>,
    body: Ruma<get_member_events::Request>,
    _room_id: String,
) -> ConduitResult<get_member_events::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(user_id, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    Ok(get_member_events::Response {
        chunk: db
            .rooms
            .room_state_type(&body.room_id, &EventType::RoomMember)?
            .values()
            .map(|pdu| pdu.to_member_event())
            .collect(),
    }
    .into())
}

#[get("/_matrix/client/r0/thirdparty/protocols")]
pub fn get_protocols_route() -> ConduitResult<get_protocols::Response> {
    warn!("TODO: get_protocols_route");
    Ok(get_protocols::Response {
        protocols: BTreeMap::new(),
    }
    .into())
}

#[put(
    "/_matrix/client/r0/rooms/<_room_id>/send/<_event_type>/<_txn_id>",
    data = "<body>"
)]
pub fn create_message_event_route(
    db: State<'_, Database>,
    body: Ruma<create_message_event::Request>,
    _room_id: String,
    _event_type: String,
    _txn_id: String,
) -> ConduitResult<create_message_event::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let mut unsigned = serde_json::Map::new();
    unsigned.insert("transaction_id".to_owned(), body.txn_id.clone().into());

    let event_id = db.rooms.append_pdu(
        body.room_id.clone(),
        user_id.clone(),
        body.event_type.clone(),
        serde_json::from_str(
            body.json_body
                .ok_or(Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?
                .get(),
        )
        .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?,
        Some(unsigned),
        None,
        None,
        &db.globals,
    )?;

    Ok(create_message_event::Response { event_id }.into())
}

#[put(
    "/_matrix/client/r0/rooms/<_room_id>/state/<_event_type>/<_state_key>",
    data = "<body>"
)]
pub fn create_state_event_for_key_route(
    db: State<'_, Database>,
    body: Ruma<create_state_event_for_key::Request>,
    _room_id: String,
    _event_type: String,
    _state_key: String,
) -> ConduitResult<create_state_event_for_key::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let content = serde_json::from_str::<serde_json::Value>(
        body.json_body
            .as_ref()
            .ok_or(Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?
            .get(),
    )
    .map_err(|_| Error::BadRequest(ErrorKind::BadJson, "Invalid JSON body."))?;

    if body.event_type == EventType::RoomCanonicalAlias {
        let canonical_alias = serde_json::from_value::<
            EventJson<canonical_alias::CanonicalAliasEventContent>,
        >(content.clone())
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid canonical alias."))?
        .deserialize()
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid canonical alias."))?;

        let mut aliases = canonical_alias.alt_aliases;

        if let Some(alias) = canonical_alias.alias {
            aliases.push(alias);
        }

        for alias in aliases {
            if alias.server_name() != db.globals.server_name()
                || db
                    .rooms
                    .id_from_alias(&alias)?
                    .filter(|room| room == &body.room_id) // Make sure it's the right room
                    .is_none()
            {
                return Err(Error::BadRequest(ErrorKind::Forbidden, "You are only allowed to send canonical_alias events when it's aliases already exists"));
            }
        }
    }

    let event_id = db.rooms.append_pdu(
        body.room_id.clone(),
        user_id.clone(),
        body.event_type.clone(),
        content,
        None,
        Some(body.state_key.clone()),
        None,
        &db.globals,
    )?;

    Ok(create_state_event_for_key::Response { event_id }.into())
}

#[put(
    "/_matrix/client/r0/rooms/<_room_id>/state/<_event_type>",
    data = "<body>"
)]
pub fn create_state_event_for_empty_key_route(
    db: State<'_, Database>,
    body: Ruma<create_state_event_for_empty_key::Request>,
    _room_id: String,
    _event_type: String,
) -> ConduitResult<create_state_event_for_empty_key::Response> {
    // This just calls create_state_event_for_key_route
    let Ruma {
        body:
            create_state_event_for_empty_key::Request {
                room_id,
                event_type,
                data,
            },
        user_id,
        device_id,
        json_body,
    } = body;

    Ok(create_state_event_for_empty_key::Response {
        event_id: create_state_event_for_key_route(
            db,
            Ruma {
                body: create_state_event_for_key::Request {
                    room_id,
                    event_type,
                    data,
                    state_key: "".to_owned(),
                },
                user_id,
                device_id,
                json_body,
            },
            _room_id,
            _event_type,
            "".to_owned(),
        )?
        .0
        .event_id,
    }
    .into())
}

#[get("/_matrix/client/r0/rooms/<_room_id>/state", data = "<body>")]
pub fn get_state_events_route(
    db: State<'_, Database>,
    body: Ruma<get_state_events::Request>,
    _room_id: String,
) -> ConduitResult<get_state_events::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(user_id, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view the room state.",
        ));
    }

    Ok(get_state_events::Response {
        room_state: db
            .rooms
            .room_state_full(&body.room_id)?
            .values()
            .map(|pdu| pdu.to_state_event())
            .collect(),
    }
    .into())
}

#[get(
    "/_matrix/client/r0/rooms/<_room_id>/state/<_event_type>/<_state_key>",
    data = "<body>"
)]
pub fn get_state_events_for_key_route(
    db: State<'_, Database>,
    body: Ruma<get_state_events_for_key::Request>,
    _room_id: String,
    _event_type: String,
    _state_key: String,
) -> ConduitResult<get_state_events_for_key::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(user_id, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view the room state.",
        ));
    }

    let event = db
        .rooms
        .room_state_get(&body.room_id, &body.event_type, &body.state_key)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "State event not found.",
        ))?;

    Ok(get_state_events_for_key::Response {
        content: serde_json::value::to_raw_value(&event.content)
            .map_err(|_| Error::bad_database("Invalid event content in database"))?,
    }
    .into())
}

#[get(
    "/_matrix/client/r0/rooms/<_room_id>/state/<_event_type>",
    data = "<body>"
)]
pub fn get_state_events_for_empty_key_route(
    db: State<'_, Database>,
    body: Ruma<get_state_events_for_empty_key::Request>,
    _room_id: String,
    _event_type: String,
) -> ConduitResult<get_state_events_for_empty_key::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(user_id, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view the room state.",
        ));
    }

    let event = db
        .rooms
        .room_state_get(&body.room_id, &body.event_type, "")?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "State event not found.",
        ))?;

    Ok(get_state_events_for_empty_key::Response {
        content: serde_json::value::to_raw_value(&event)
            .map_err(|_| Error::bad_database("Invalid event content in database"))?,
    }
    .into())
}

#[get("/_matrix/client/r0/sync", data = "<body>")]
pub fn sync_route(
    db: State<'_, Database>,
    body: Ruma<sync_events::Request>,
) -> ConduitResult<sync_events::Response> {
    std::thread::sleep(Duration::from_millis(1000));
    let user_id = body.user_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

    let next_batch = db.globals.current_count()?.to_string();

    let mut joined_rooms = BTreeMap::new();
    let since = body
        .since
        .clone()
        .and_then(|string| string.parse().ok())
        .unwrap_or(0);

    for room_id in db.rooms.rooms_joined(&user_id) {
        let room_id = room_id?;

        let mut pdus = db
            .rooms
            .pdus_since(&user_id, &room_id, since)?
            .filter_map(|r| r.ok()) // Filter out buggy events
            .collect::<Vec<_>>();

        let mut send_member_count = false;
        let mut joined_since_last_sync = false;
        let mut send_notification_counts = false;
        for pdu in &pdus {
            send_notification_counts = true;
            if pdu.kind == EventType::RoomMember {
                send_member_count = true;
                if !joined_since_last_sync && pdu.state_key == Some(user_id.to_string()) {
                    let content = serde_json::from_value::<
                        EventJson<ruma::events::room::member::MemberEventContent>,
                    >(pdu.content.clone())
                    .expect("EventJson::from_value always works")
                    .deserialize()
                    .map_err(|_| Error::bad_database("Invalid PDU in database."))?;
                    if content.membership == ruma::events::room::member::MembershipState::Join {
                        joined_since_last_sync = true;
                        // Both send_member_count and joined_since_last_sync are set. There's nothing more
                        // to do
                        break;
                    }
                }
            }
        }

        let members = db.rooms.room_state_type(&room_id, &EventType::RoomMember)?;

        let (joined_member_count, invited_member_count, heroes) = if send_member_count {
            let joined_member_count = db.rooms.room_members(&room_id).count();
            let invited_member_count = db.rooms.room_members_invited(&room_id).count();

            // Recalculate heroes (first 5 members)
            let mut heroes = Vec::new();

            if joined_member_count + invited_member_count <= 5 {
                // Go through all PDUs and for each member event, check if the user is still joined or
                // invited until we have 5 or we reach the end

                for hero in db
                    .rooms
                    .all_pdus(&user_id, &room_id)?
                    .filter_map(|pdu| pdu.ok()) // Ignore all broken pdus
                    .filter(|pdu| pdu.kind == EventType::RoomMember)
                    .map(|pdu| {
                        let content = serde_json::from_value::<
                            EventJson<ruma::events::room::member::MemberEventContent>,
                        >(pdu.content.clone())
                        .map_err(|_| Error::bad_database("Invalid member event in database."))?
                        .deserialize()
                        .map_err(|_| Error::bad_database("Invalid member event in database."))?;

                        if let Some(state_key) = &pdu.state_key {
                            let current_content = serde_json::from_value::<
                                EventJson<ruma::events::room::member::MemberEventContent>,
                            >(
                                members
                                    .get(state_key)
                                    .ok_or_else(|| {
                                        Error::bad_database(
                                            "A user that joined once has no member event anymore.",
                                        )
                                    })?
                                    .content
                                    .clone(),
                            )
                            .map_err(|_| Error::bad_database("Invalid member event in database."))?
                            .deserialize()
                            .map_err(|_| {
                                Error::bad_database("Invalid member event in database.")
                            })?;

                            // The membership was and still is invite or join
                            if matches!(
                                content.membership,
                                ruma::events::room::member::MembershipState::Join
                                    | ruma::events::room::member::MembershipState::Invite
                            ) && matches!(
                                current_content.membership,
                                ruma::events::room::member::MembershipState::Join
                                    | ruma::events::room::member::MembershipState::Invite
                            ) {
                                Ok::<_, Error>(Some(state_key.clone()))
                            } else {
                                Ok(None)
                            }
                        } else {
                            Ok(None)
                        }
                    })
                    .filter_map(|u| u.ok()) // Filter out buggy users
                    // Filter for possible heroes
                    .filter_map(|u| u)
                {
                    if heroes.contains(&hero) || hero == user_id.to_string() {
                        continue;
                    }

                    heroes.push(hero);
                }
            }

            (
                Some(joined_member_count),
                Some(invited_member_count),
                heroes,
            )
        } else {
            (None, None, Vec::new())
        };

        let notification_count = if send_notification_counts {
            if let Some(last_read) = db.rooms.edus.room_read_get(&room_id, &user_id)? {
                Some(
                    (db.rooms
                        .pdus_since(&user_id, &room_id, last_read)?
                        .filter_map(|pdu| pdu.ok()) // Filter out buggy events
                        .filter(|pdu| {
                            matches!(
                                pdu.kind.clone(),
                                EventType::RoomMessage | EventType::RoomEncrypted
                            )
                        })
                        .count() as u32)
                        .into(),
                )
            } else {
                None
            }
        } else {
            None
        };

        // They /sync response doesn't always return all messages, so we say the output is
        // limited unless there are enough events
        let mut limited = true;
        pdus = pdus.split_off(pdus.len().checked_sub(10).unwrap_or_else(|| {
            limited = false;
            0
        }));

        let prev_batch = pdus.first().map_or(Ok::<_, Error>(None), |e| {
            Ok(Some(
                db.rooms
                    .get_pdu_count(&e.event_id)?
                    .ok_or_else(|| Error::bad_database("Can't find count from event in db."))?
                    .to_string(),
            ))
        })?;

        let room_events = pdus
            .into_iter()
            .map(|pdu| pdu.to_room_event())
            .collect::<Vec<_>>();

        let mut edus = db
            .rooms
            .edus
            .roomlatests_since(&room_id, since)?
            .filter_map(|r| r.ok()) // Filter out buggy events
            .collect::<Vec<_>>();

        if db
            .rooms
            .edus
            .last_roomactive_update(&room_id, &db.globals)?
            > since
        {
            edus.push(
                serde_json::from_str(
                    &serde_json::to_string(&EduEvent::Typing(
                        db.rooms.edus.roomactives_all(&room_id)?,
                    ))
                    .expect("event is valid, we just created it"),
                )
                .expect("event is valid, we just created it"),
            );
        }

        let joined_room = sync_events::JoinedRoom {
            account_data: sync_events::AccountData {
                events: db
                    .account_data
                    .changes_since(Some(&room_id), &user_id, since)?
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect(),
            },
            summary: sync_events::RoomSummary {
                heroes,
                joined_member_count: joined_member_count.map(|n| (n as u32).into()),
                invited_member_count: invited_member_count.map(|n| (n as u32).into()),
            },
            unread_notifications: sync_events::UnreadNotificationsCount {
                highlight_count: None,
                notification_count,
            },
            timeline: sync_events::Timeline {
                limited: if limited || joined_since_last_sync {
                    Some(true)
                } else {
                    None
                },
                prev_batch,
                events: room_events,
            },
            // TODO: state before timeline
            state: sync_events::State {
                events: if joined_since_last_sync {
                    db.rooms
                        .room_state_full(&room_id)?
                        .into_iter()
                        .map(|(_, pdu)| pdu.to_state_event())
                        .collect()
                } else {
                    Vec::new()
                },
            },
            ephemeral: sync_events::Ephemeral { events: edus },
        };

        if !joined_room.is_empty() {
            joined_rooms.insert(room_id.clone(), joined_room);
        }
    }

    let mut left_rooms = BTreeMap::new();
    for room_id in db.rooms.rooms_left(&user_id) {
        let room_id = room_id?;
        let pdus = db.rooms.pdus_since(&user_id, &room_id, since)?;
        let room_events = pdus
            .filter_map(|pdu| pdu.ok()) // Filter out buggy events
            .map(|pdu| pdu.to_room_event())
            .collect();

        // TODO: Only until leave point
        let mut edus = db
            .rooms
            .edus
            .roomlatests_since(&room_id, since)?
            .filter_map(|r| r.ok()) // Filter out buggy events
            .collect::<Vec<_>>();

        if db
            .rooms
            .edus
            .last_roomactive_update(&room_id, &db.globals)?
            > since
        {
            edus.push(
                serde_json::from_str(
                    &serde_json::to_string(&EduEvent::Typing(
                        db.rooms.edus.roomactives_all(&room_id)?,
                    ))
                    .expect("event is valid, we just created it"),
                )
                .expect("event is valid, we just created it"),
            );
        }

        let left_room = sync_events::LeftRoom {
            account_data: sync_events::AccountData { events: Vec::new() },
            timeline: sync_events::Timeline {
                limited: Some(false),
                prev_batch: Some(next_batch.clone()),
                events: room_events,
            },
            state: sync_events::State { events: Vec::new() },
        };

        if !left_room.is_empty() {
            left_rooms.insert(room_id.clone(), left_room);
        }
    }

    let mut invited_rooms = BTreeMap::new();
    for room_id in db.rooms.rooms_invited(&user_id) {
        let room_id = room_id?;

        let invited_room = sync_events::InvitedRoom {
            invite_state: sync_events::InviteState {
                events: db
                    .rooms
                    .room_state_full(&room_id)?
                    .into_iter()
                    .map(|(_, pdu)| pdu.to_stripped_state_event())
                    .collect(),
            },
        };

        if !invited_room.is_empty() {
            invited_rooms.insert(room_id.clone(), invited_room);
        }
    }

    Ok(sync_events::Response {
        next_batch,
        rooms: sync_events::Rooms {
            leave: left_rooms,
            join: joined_rooms,
            invite: invited_rooms,
        },
        presence: sync_events::Presence {
            events: db
                .global_edus
                .presence_since(since)?
                .map(|edu| {
                    let mut edu = edu?
                        .deserialize()
                        .map_err(|_| Error::bad_database("EDU in database is invalid."))?;
                    if let Some(timestamp) = edu.content.last_active_ago {
                        let last_active_ago =
                            js_int::UInt::try_from(utils::millis_since_unix_epoch())
                                .expect("time is valid")
                                - timestamp;
                        edu.content.last_active_ago = Some(last_active_ago);
                    }
                    Ok::<_, Error>(edu.into())
                })
                .filter_map(|edu| edu.ok()) // Filter out buggy events
                .collect(),
        },
        account_data: sync_events::AccountData {
            events: db
                .account_data
                .changes_since(None, &user_id, since)?
                .into_iter()
                .map(|(_, v)| v)
                .collect(),
        },
        device_lists: sync_events::DeviceLists {
            changed: if since != 0 {
                db.users
                    .keys_changed(since)
                    .filter_map(|u| u.ok())
                    .collect() // Filter out buggy events
            } else {
                Vec::new()
            },
            left: Vec::new(), // TODO
        },
        device_one_time_keys_count: Default::default(), // TODO
        to_device: sync_events::ToDevice {
            events: db.users.take_to_device_events(user_id, device_id, 100)?,
        },
    }
    .into())
}

#[get(
    "/_matrix/client/r0/rooms/<_room_id>/context/<_event_id>",
    data = "<body>"
)]
pub fn get_context_route(
    db: State<'_, Database>,
    body: Ruma<get_context::Request>,
    _room_id: String,
    _event_id: String,
) -> ConduitResult<get_context::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(user_id, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    let base_event = db
        .rooms
        .get_pdu(&body.event_id)?
        .ok_or(Error::BadRequest(
            ErrorKind::NotFound,
            "Base event not found.",
        ))?
        .to_room_event();

    let base_token = db
        .rooms
        .get_pdu_count(&body.event_id)?
        .expect("event still exists");

    let events_before = db
        .rooms
        .pdus_until(&user_id, &body.room_id, base_token)
        .take(
            u32::try_from(body.limit).map_err(|_| {
                Error::BadRequest(ErrorKind::InvalidParam, "Limit value is invalid.")
            })? as usize
                / 2,
        )
        .filter_map(|r| r.ok()) // Remove buggy events
        .collect::<Vec<_>>();

    let start_token = events_before.last().map_or(Ok(None), |e| {
        Ok::<_, Error>(Some(
            db.rooms
                .get_pdu_count(&e.event_id)?
                .ok_or_else(|| Error::bad_database("Can't find count from event in db."))?
                .to_string(),
        ))
    })?;

    let events_before = events_before
        .into_iter()
        .map(|pdu| pdu.to_room_event())
        .collect::<Vec<_>>();

    let events_after = db
        .rooms
        .pdus_after(&user_id, &body.room_id, base_token)
        .take(
            u32::try_from(body.limit).map_err(|_| {
                Error::BadRequest(ErrorKind::InvalidParam, "Limit value is invalid.")
            })? as usize
                / 2,
        )
        .filter_map(|r| r.ok()) // Remove buggy events
        .collect::<Vec<_>>();

    let end_token = events_after.last().map_or(Ok(None), |e| {
        Ok::<_, Error>(Some(
            db.rooms
                .get_pdu_count(&e.event_id)?
                .ok_or_else(|| Error::bad_database("Can't find count from event in db."))?
                .to_string(),
        ))
    })?;

    let events_after = events_after
        .into_iter()
        .map(|pdu| pdu.to_room_event())
        .collect::<Vec<_>>();

    Ok(get_context::Response {
        start: start_token,
        end: end_token,
        events_before,
        event: Some(base_event),
        events_after,
        state: db // TODO: State at event
            .rooms
            .room_state_full(&body.room_id)?
            .values()
            .map(|pdu| pdu.to_state_event())
            .collect(),
    }
    .into())
}

#[get("/_matrix/client/r0/rooms/<_room_id>/messages", data = "<body>")]
pub fn get_message_events_route(
    db: State<'_, Database>,
    body: Ruma<get_message_events::Request>,
    _room_id: String,
) -> ConduitResult<get_message_events::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    if !db.rooms.is_joined(user_id, &body.room_id)? {
        return Err(Error::BadRequest(
            ErrorKind::Forbidden,
            "You don't have permission to view this room.",
        ));
    }

    let from = body
        .from
        .clone()
        .parse()
        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid `from` value."))?;
    match body.dir {
        get_message_events::Direction::Forward => {
            let events_after = db
                .rooms
                .pdus_after(&user_id, &body.room_id, from)
                // Use limit or else 10
                .take(body.limit.map_or(Ok::<_, Error>(10_usize), |l| {
                    Ok(u32::try_from(l).map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidParam, "Limit value is invalid.")
                    })? as usize)
                })?)
                .filter_map(|r| r.ok()) // Filter out buggy events
                .collect::<Vec<_>>();

            let end_token = events_after.last().map_or(Ok::<_, Error>(None), |e| {
                Ok(Some(
                    db.rooms
                        .get_pdu_count(&e.event_id)?
                        .ok_or_else(|| Error::bad_database("Can't find count from event in db."))?
                        .to_string(),
                ))
            })?;

            let events_after = events_after
                .into_iter()
                .map(|pdu| pdu.to_room_event())
                .collect::<Vec<_>>();

            Ok(get_message_events::Response {
                start: Some(body.from.clone()),
                end: end_token,
                chunk: events_after,
                state: Vec::new(),
            }
            .into())
        }
        get_message_events::Direction::Backward => {
            let events_before = db
                .rooms
                .pdus_until(&user_id, &body.room_id, from)
                // Use limit or else 10
                .take(body.limit.map_or(Ok::<_, Error>(10_usize), |l| {
                    Ok(u32::try_from(l).map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidParam, "Limit value is invalid.")
                    })? as usize)
                })?)
                .filter_map(|r| r.ok()) // Filter out buggy events
                .collect::<Vec<_>>();

            let start_token = events_before.last().map_or(Ok::<_, Error>(None), |e| {
                Ok(Some(
                    db.rooms
                        .get_pdu_count(&e.event_id)?
                        .ok_or_else(|| Error::bad_database("Can't find count from event in db."))?
                        .to_string(),
                ))
            })?;

            let events_before = events_before
                .into_iter()
                .map(|pdu| pdu.to_room_event())
                .collect::<Vec<_>>();

            Ok(get_message_events::Response {
                start: Some(body.from.clone()),
                end: start_token,
                chunk: events_before,
                state: Vec::new(),
            }
            .into())
        }
    }
}

#[get("/_matrix/client/r0/voip/turnServer")]
pub fn turn_server_route() -> ConduitResult<create_message_event::Response> {
    Err(Error::BadRequest(
        ErrorKind::NotFound,
        "There is no turn server yet.",
    ))
}

#[post("/_matrix/client/r0/publicised_groups")]
pub fn publicised_groups_route() -> ConduitResult<create_message_event::Response> {
    Err(Error::BadRequest(
        ErrorKind::NotFound,
        "There are not publicised groups yet.",
    ))
}

#[put(
    "/_matrix/client/r0/sendToDevice/<_event_type>/<_txn_id>",
    data = "<body>"
)]
pub fn send_event_to_device_route(
    db: State<'_, Database>,
    body: Ruma<send_event_to_device::Request>,
    _event_type: String,
    _txn_id: String,
) -> ConduitResult<send_event_to_device::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    for (target_user_id, map) in &body.messages {
        for (target_device_id_maybe, event) in map {
            match target_device_id_maybe {
                to_device::DeviceIdOrAllDevices::DeviceId(target_device_id) => {
                    db.users.add_to_device_event(
                        user_id,
                        &target_user_id,
                        &target_device_id,
                        &body.event_type,
                        serde_json::from_str(event.get()).map_err(|_| {
                            Error::BadRequest(ErrorKind::InvalidParam, "Event is invalid")
                        })?,
                        &db.globals,
                    )?
                }

                to_device::DeviceIdOrAllDevices::AllDevices => {
                    for target_device_id in db.users.all_device_ids(&target_user_id) {
                        db.users.add_to_device_event(
                            user_id,
                            &target_user_id,
                            &target_device_id?,
                            &body.event_type,
                            serde_json::from_str(event.get()).map_err(|_| {
                                Error::BadRequest(ErrorKind::InvalidParam, "Event is invalid")
                            })?,
                            &db.globals,
                        )?;
                    }
                }
            }
        }
    }

    Ok(send_event_to_device::Response.into())
}

#[get("/_matrix/media/r0/config")]
pub fn get_media_config_route() -> ConduitResult<get_media_config::Response> {
    Ok(get_media_config::Response {
        upload_size: (20_u32 * 1024 * 1024).into(), // 20 MB
    }
    .into())
}

#[post("/_matrix/media/r0/upload", data = "<body>")]
pub fn create_content_route(
    db: State<'_, Database>,
    body: Ruma<create_content::Request>,
) -> ConduitResult<create_content::Response> {
    let mxc = format!(
        "mxc://{}/{}",
        db.globals.server_name(),
        utils::random_string(MXC_LENGTH)
    );
    db.media.create(
        mxc.clone(),
        body.filename.as_ref(),
        &body.content_type,
        &body.file,
    )?;

    Ok(create_content::Response { content_uri: mxc }.into())
}

#[get(
    "/_matrix/media/r0/download/<_server_name>/<_media_id>",
    data = "<body>"
)]
pub fn get_content_route(
    db: State<'_, Database>,
    body: Ruma<get_content::Request>,
    _server_name: String,
    _media_id: String,
) -> ConduitResult<get_content::Response> {
    if let Some((filename, content_type, file)) = db
        .media
        .get(format!("mxc://{}/{}", body.server_name, body.media_id))?
    {
        Ok(get_content::Response {
            file,
            content_type,
            content_disposition: filename.unwrap_or_default(), // TODO: Spec says this should be optional
        }
        .into())
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}

#[get(
    "/_matrix/media/r0/thumbnail/<_server_name>/<_media_id>",
    data = "<body>"
)]
pub fn get_content_thumbnail_route(
    db: State<'_, Database>,
    body: Ruma<get_content_thumbnail::Request>,
    _server_name: String,
    _media_id: String,
) -> ConduitResult<get_content_thumbnail::Response> {
    if let Some((_, content_type, file)) = db.media.get_thumbnail(
        format!("mxc://{}/{}", body.server_name, body.media_id),
        body.width
            .try_into()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
        body.height
            .try_into()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
    )? {
        Ok(get_content_thumbnail::Response { file, content_type }.into())
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}

#[get("/_matrix/client/r0/devices", data = "<body>")]
pub fn get_devices_route(
    db: State<'_, Database>,
    body: Ruma<get_devices::Request>,
) -> ConduitResult<get_devices::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let devices = db
        .users
        .all_devices_metadata(user_id)
        .filter_map(|r| r.ok()) // Filter out buggy devices
        .collect::<Vec<device::Device>>();

    Ok(get_devices::Response { devices }.into())
}

#[get("/_matrix/client/r0/devices/<_device_id>", data = "<body>")]
pub fn get_device_route(
    db: State<'_, Database>,
    body: Ruma<get_device::Request>,
    _device_id: String,
) -> ConduitResult<get_device::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let device = db
        .users
        .get_device_metadata(&user_id, &body.body.device_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Device not found."))?;

    Ok(get_device::Response { device }.into())
}

#[put("/_matrix/client/r0/devices/<_device_id>", data = "<body>")]
pub fn update_device_route(
    db: State<'_, Database>,
    body: Ruma<update_device::Request>,
    _device_id: String,
) -> ConduitResult<update_device::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");

    let mut device = db
        .users
        .get_device_metadata(&user_id, &body.body.device_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Device not found."))?;

    device.display_name = body.display_name.clone();

    db.users
        .update_device_metadata(&user_id, &body.body.device_id, &device)?;

    Ok(update_device::Response.into())
}

#[delete("/_matrix/client/r0/devices/<_device_id>", data = "<body>")]
pub fn delete_device_route(
    db: State<'_, Database>,
    body: Ruma<delete_device::Request>,
    _device_id: String,
) -> ConduitResult<delete_device::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

    // UIAA
    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec!["m.login.password".to_owned()],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) = db.uiaa.try_auth(
            &user_id,
            &device_id,
            auth,
            &uiaainfo,
            &db.users,
            &db.globals,
        )?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa.create(&user_id, &device_id, &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
    }

    db.users.remove_device(&user_id, &body.body.device_id)?;

    Ok(delete_device::Response.into())
}

#[post("/_matrix/client/r0/delete_devices", data = "<body>")]
pub fn delete_devices_route(
    db: State<'_, Database>,
    body: Ruma<delete_devices::Request>,
) -> ConduitResult<delete_devices::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

    // UIAA
    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec!["m.login.password".to_owned()],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) = db.uiaa.try_auth(
            &user_id,
            &device_id,
            auth,
            &uiaainfo,
            &db.users,
            &db.globals,
        )?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa.create(&user_id, &device_id, &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
    }

    for device_id in &body.devices {
        db.users.remove_device(&user_id, &device_id)?
    }

    Ok(delete_devices::Response.into())
}

#[post("/_matrix/client/unstable/keys/device_signing/upload", data = "<body>")]
pub fn upload_signing_keys_route(
    db: State<'_, Database>,
    body: Ruma<upload_signing_keys::Request>,
) -> ConduitResult<upload_signing_keys::Response> {
    let user_id = body.user_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

    // UIAA
    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec!["m.login.password".to_owned()],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) = db.uiaa.try_auth(
            &user_id,
            &device_id,
            auth,
            &uiaainfo,
            &db.users,
            &db.globals,
        )?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa.create(&user_id, &device_id, &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
    }

    if let Some(master_key) = &body.master_key {
        db.users.add_cross_signing_keys(
            user_id,
            &master_key,
            &body.self_signing_key,
            &body.user_signing_key,
            &db.globals,
        )?;
    }

    Ok(upload_signing_keys::Response.into())
}

#[post("/_matrix/client/unstable/keys/signatures/upload", data = "<body>")]
pub fn upload_signatures_route(
    db: State<'_, Database>,
    body: Ruma<upload_signatures::Request>,
) -> ConduitResult<upload_signatures::Response> {
    let sender_id = body.user_id.as_ref().expect("user is authenticated");

    for (user_id, signed_keys) in &body.signed_keys {
        for (key_id, signed_key) in signed_keys {
            for signature in signed_key
                .get("signatures")
                .ok_or(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Missing signatures field.",
                ))?
                .get(sender_id.to_string())
                .ok_or(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Invalid user in signatures field.",
                ))?
                .as_object()
                .ok_or(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Invalid signature.",
                ))?
                .clone()
                .into_iter()
            {
                // Signature validation?
                let signature = (
                    signature.0,
                    signature
                        .1
                        .as_str()
                        .ok_or(Error::BadRequest(
                            ErrorKind::InvalidParam,
                            "Invalid signature value.",
                        ))?
                        .to_owned(),
                );
                db.users
                    .sign_key(&user_id, &key_id, signature, &sender_id, &db.globals)?;
            }
        }
    }

    Ok(upload_signatures::Response.into())
}

#[get("/_matrix/client/r0/pushers")]
pub fn pushers_route() -> ConduitResult<get_pushers::Response> {
    Ok(get_pushers::Response {
        pushers: Vec::new(),
    }
    .into())
}

#[post("/_matrix/client/r0/pushers/set")]
pub fn set_pushers_route() -> ConduitResult<get_pushers::Response> {
    Ok(get_pushers::Response {
        pushers: Vec::new(),
    }
    .into())
}

#[options("/<_segments..>")]
pub fn options_route(
    _segments: rocket::http::uri::Segments<'_>,
) -> ConduitResult<send_event_to_device::Response> {
    Ok(send_event_to_device::Response.into())
}
