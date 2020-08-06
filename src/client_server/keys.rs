use super::{State, SESSION_ID_LENGTH};
use crate::{utils, ConduitResult, Database, Error, Ruma};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::{
            keys::{
                claim_keys, get_key_changes, get_keys, upload_keys, upload_signatures,
                upload_signing_keys,
            },
            uiaa::{AuthFlow, UiaaInfo},
        },
    },
    encryption::UnsignedDeviceInfo,
};
use std::collections::{BTreeMap, HashSet};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/keys/upload", data = "<body>")
)]
pub fn upload_keys_route(
    db: State<'_, Database>,
    body: Ruma<upload_keys::Request>,
) -> ConduitResult<upload_keys::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");
    let device_id = body.device_id.as_ref().expect("user is authenticated");

    if let Some(one_time_keys) = &body.one_time_keys {
        for (key_key, key_value) in one_time_keys {
            db.users
                .add_one_time_key(sender_id, device_id, key_key, key_value, &db.globals)?;
        }
    }

    if let Some(device_keys) = &body.device_keys {
        // This check is needed to assure that signatures are kept
        if db.users.get_device_keys(sender_id, device_id)?.is_none() {
            db.users
                .add_device_keys(sender_id, device_id, device_keys, &db.rooms, &db.globals)?;
        }
    }

    Ok(upload_keys::Response {
        one_time_key_counts: db.users.count_one_time_keys(sender_id, device_id)?,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/keys/query", data = "<body>")
)]
pub fn get_keys_route(
    db: State<'_, Database>,
    body: Ruma<get_keys::IncomingRequest>,
) -> ConduitResult<get_keys::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

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

                    keys.unsigned = Some(UnsignedDeviceInfo {
                        device_display_name: metadata.display_name,
                    });

                    container.insert(device_id, keys);
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

                    keys.unsigned = Some(UnsignedDeviceInfo {
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

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/keys/claim", data = "<body>")
)]
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
                    .take_one_time_key(user_id, device_id, key_algorithm, &db.globals)?
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

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/unstable/keys/device_signing/upload", data = "<body>")
)]
pub fn upload_signing_keys_route(
    db: State<'_, Database>,
    body: Ruma<upload_signing_keys::IncomingRequest>,
) -> ConduitResult<upload_signing_keys::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");
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
            &sender_id,
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
        db.uiaa.create(&sender_id, &device_id, &uiaainfo)?;
        return Err(Error::Uiaa(uiaainfo));
    }

    if let Some(master_key) = &body.master_key {
        db.users.add_cross_signing_keys(
            sender_id,
            &master_key,
            &body.self_signing_key,
            &body.user_signing_key,
            &db.rooms,
            &db.globals,
        )?;
    }

    Ok(upload_signing_keys::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/unstable/keys/signatures/upload", data = "<body>")
)]
pub fn upload_signatures_route(
    db: State<'_, Database>,
    body: Ruma<upload_signatures::Request>,
) -> ConduitResult<upload_signatures::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

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
                db.users.sign_key(
                    &user_id,
                    &key_id,
                    signature,
                    &sender_id,
                    &db.rooms,
                    &db.globals,
                )?;
            }
        }
    }

    Ok(upload_signatures::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/keys/changes", data = "<body>")
)]
pub fn get_key_changes_route(
    db: State<'_, Database>,
    body: Ruma<get_key_changes::IncomingRequest>,
) -> ConduitResult<get_key_changes::Response> {
    let sender_id = body.sender_id.as_ref().expect("user is authenticated");

    let mut device_list_updates = HashSet::new();

    device_list_updates.extend(
        db.users
            .keys_changed(
                &sender_id.to_string(),
                body.from
                    .parse()
                    .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid `from`."))?,
                Some(
                    body.to
                        .parse()
                        .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Invalid `to`."))?,
                ),
            )
            .filter_map(|r| r.ok()),
    );

    for room_id in db.rooms.rooms_joined(sender_id).filter_map(|r| r.ok()) {
        device_list_updates.extend(
            db.users
                .keys_changed(
                    &room_id.to_string(),
                    body.from.parse().map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidParam, "Invalid `from`.")
                    })?,
                    Some(body.to.parse().map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidParam, "Invalid `to`.")
                    })?),
                )
                .filter_map(|r| r.ok()),
        );
    }
    Ok(get_key_changes::Response {
        changed: device_list_updates.into_iter().collect(),
        left: Vec::new(), // TODO
    }
    .into())
}
