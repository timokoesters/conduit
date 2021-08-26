use super::SESSION_ID_LENGTH;
use crate::{database::DatabaseGuard, utils, ConduitResult, Database, Error, Result, Ruma};
use rocket::futures::{prelude::*, stream::FuturesUnordered};
use ruma::{
    api::{
        client::{
            error::ErrorKind,
            r0::{
                keys::{
                    claim_keys, get_key_changes, get_keys, upload_keys, upload_signatures,
                    upload_signing_keys,
                },
                uiaa::{AuthFlow, UiaaInfo},
            },
        },
        federation,
    },
    encryption::UnsignedDeviceInfo,
    DeviceId, DeviceKeyAlgorithm, UserId,
};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/keys/upload", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn upload_keys_route(
    db: DatabaseGuard,
    body: Ruma<upload_keys::Request>,
) -> ConduitResult<upload_keys::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    if let Some(one_time_keys) = &body.one_time_keys {
        for (key_key, key_value) in one_time_keys {
            db.users.add_one_time_key(
                sender_user,
                sender_device,
                key_key,
                key_value,
                &db.globals,
            )?;
        }
    }

    if let Some(device_keys) = &body.device_keys {
        // This check is needed to assure that signatures are kept
        if db
            .users
            .get_device_keys(sender_user, sender_device)?
            .is_none()
        {
            db.users.add_device_keys(
                sender_user,
                sender_device,
                device_keys,
                &db.rooms,
                &db.globals,
            )?;
        }
    }

    db.flush()?;

    Ok(upload_keys::Response {
        one_time_key_counts: db.users.count_one_time_keys(sender_user, sender_device)?,
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/keys/query", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_keys_route(
    db: DatabaseGuard,
    body: Ruma<get_keys::Request<'_>>,
) -> ConduitResult<get_keys::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let response = get_keys_helper(
        Some(sender_user),
        &body.device_keys,
        |u| u == sender_user,
        &db,
    )
    .await?;

    Ok(response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/keys/claim", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn claim_keys_route(
    db: DatabaseGuard,
    body: Ruma<claim_keys::Request>,
) -> ConduitResult<claim_keys::Response> {
    let response = claim_keys_helper(&body.one_time_keys, &db).await?;

    db.flush()?;

    Ok(response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/unstable/keys/device_signing/upload", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn upload_signing_keys_route(
    db: DatabaseGuard,
    body: Ruma<upload_signing_keys::Request<'_>>,
) -> ConduitResult<upload_signing_keys::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

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
            &sender_user,
            &sender_device,
            auth,
            &uiaainfo,
            &db.users,
            &db.globals,
        )?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else if let Some(json) = body.json_body {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa
            .create(&sender_user, &sender_device, &uiaainfo, &json)?;
        return Err(Error::Uiaa(uiaainfo));
    } else {
        return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
    }

    if let Some(master_key) = &body.master_key {
        db.users.add_cross_signing_keys(
            sender_user,
            &master_key,
            &body.self_signing_key,
            &body.user_signing_key,
            &db.rooms,
            &db.globals,
        )?;
    }

    db.flush()?;

    Ok(upload_signing_keys::Response {}.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/unstable/keys/signatures/upload", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn upload_signatures_route(
    db: DatabaseGuard,
    body: Ruma<upload_signatures::Request>,
) -> ConduitResult<upload_signatures::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    for (user_id, signed_keys) in &body.signed_keys {
        for (key_id, signed_key) in signed_keys {
            for signature in signed_key
                .get("signatures")
                .ok_or(Error::BadRequest(
                    ErrorKind::InvalidParam,
                    "Missing signatures field.",
                ))?
                .get(sender_user.to_string())
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
                    &sender_user,
                    &db.rooms,
                    &db.globals,
                )?;
            }
        }
    }

    db.flush()?;

    Ok(upload_signatures::Response {}.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/keys/changes", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_key_changes_route(
    db: DatabaseGuard,
    body: Ruma<get_key_changes::Request<'_>>,
) -> ConduitResult<get_key_changes::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut device_list_updates = HashSet::new();

    device_list_updates.extend(
        db.users
            .keys_changed(
                &sender_user.to_string(),
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

    for room_id in db.rooms.rooms_joined(sender_user).filter_map(|r| r.ok()) {
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

pub async fn get_keys_helper<F: Fn(&UserId) -> bool>(
    sender_user: Option<&UserId>,
    device_keys_input: &BTreeMap<UserId, Vec<Box<DeviceId>>>,
    allowed_signatures: F,
    db: &Database,
) -> Result<get_keys::Response> {
    let mut master_keys = BTreeMap::new();
    let mut self_signing_keys = BTreeMap::new();
    let mut user_signing_keys = BTreeMap::new();
    let mut device_keys = BTreeMap::new();

    let mut get_over_federation = HashMap::new();

    for (user_id, device_ids) in device_keys_input {
        if user_id.server_name() != db.globals.server_name() {
            get_over_federation
                .entry(user_id.server_name())
                .or_insert_with(Vec::new)
                .push((user_id, device_ids));
            continue;
        }

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

                    keys.unsigned = UnsignedDeviceInfo {
                        device_display_name: metadata.display_name,
                    };

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

                    keys.unsigned = UnsignedDeviceInfo {
                        device_display_name: metadata.display_name,
                    };

                    container.insert(device_id.clone(), keys);
                }
                device_keys.insert(user_id.clone(), container);
            }
        }

        if let Some(master_key) = db.users.get_master_key(user_id, &allowed_signatures)? {
            master_keys.insert(user_id.clone(), master_key);
        }
        if let Some(self_signing_key) = db
            .users
            .get_self_signing_key(user_id, &allowed_signatures)?
        {
            self_signing_keys.insert(user_id.clone(), self_signing_key);
        }
        if Some(user_id) == sender_user {
            if let Some(user_signing_key) = db.users.get_user_signing_key(user_id)? {
                user_signing_keys.insert(user_id.clone(), user_signing_key);
            }
        }
    }

    let mut failures = BTreeMap::new();

    let mut futures = get_over_federation
        .into_iter()
        .map(|(server, vec)| async move {
            let mut device_keys_input_fed = BTreeMap::new();
            for (user_id, keys) in vec {
                device_keys_input_fed.insert(user_id.clone(), keys.clone());
            }
            (
                server,
                db.sending
                    .send_federation_request(
                        &db.globals,
                        server,
                        federation::keys::get_keys::v1::Request {
                            device_keys: device_keys_input_fed,
                        },
                    )
                    .await,
            )
        })
        .collect::<FuturesUnordered<_>>();

    while let Some((server, response)) = futures.next().await {
        match response {
            Ok(response) => {
                master_keys.extend(response.master_keys);
                self_signing_keys.extend(response.self_signing_keys);
                device_keys.extend(response.device_keys);
            }
            Err(_e) => {
                failures.insert(server.to_string(), json!({}));
            }
        }
    }

    Ok(get_keys::Response {
        master_keys,
        self_signing_keys,
        user_signing_keys,
        device_keys,
        failures,
    })
}

pub async fn claim_keys_helper(
    one_time_keys_input: &BTreeMap<UserId, BTreeMap<Box<DeviceId>, DeviceKeyAlgorithm>>,
    db: &Database,
) -> Result<claim_keys::Response> {
    let mut one_time_keys = BTreeMap::new();

    let mut get_over_federation = BTreeMap::new();

    for (user_id, map) in one_time_keys_input {
        if user_id.server_name() != db.globals.server_name() {
            get_over_federation
                .entry(user_id.server_name())
                .or_insert_with(Vec::new)
                .push((user_id, map));
        }

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

    let mut failures = BTreeMap::new();

    for (server, vec) in get_over_federation {
        let mut one_time_keys_input_fed = BTreeMap::new();
        for (user_id, keys) in vec {
            one_time_keys_input_fed.insert(user_id.clone(), keys.clone());
        }
        // Ignore failures
        if let Ok(keys) = db
            .sending
            .send_federation_request(
                &db.globals,
                server,
                federation::keys::claim_keys::v1::Request {
                    one_time_keys: one_time_keys_input_fed,
                },
            )
            .await
        {
            one_time_keys.extend(keys.one_time_keys);
        } else {
            failures.insert(server.to_string(), json!({}));
        }
    }

    Ok(claim_keys::Response {
        failures,
        one_time_keys,
    })
}
