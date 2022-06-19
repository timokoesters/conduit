use super::SESSION_ID_LENGTH;
use crate::{database::DatabaseGuard, utils, Database, Error, Result, Ruma};
use futures_util::{stream::FuturesUnordered, StreamExt};
use ruma::{
    api::{
        client::{
            error::ErrorKind,
            keys::{
                claim_keys, get_key_changes, get_keys, upload_keys, upload_signatures,
                upload_signing_keys,
            },
            uiaa::{AuthFlow, AuthType, UiaaInfo},
        },
        federation,
    },
    serde::Raw,
    DeviceId, DeviceKeyAlgorithm, UserId,
};
use serde_json::json;
use std::collections::{BTreeMap, HashMap, HashSet};

/// # `POST /_matrix/client/r0/keys/upload`
///
/// Publish end-to-end encryption keys for the sender device.
///
/// - Adds one time keys
/// - If there are no device keys yet: Adds device keys (TODO: merge with existing keys?)
pub async fn upload_keys_route(
    db: DatabaseGuard,
    body: Ruma<upload_keys::v3::Request>,
) -> Result<upload_keys::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    for (key_key, key_value) in &body.one_time_keys {
        db.users
            .add_one_time_key(sender_user, sender_device, key_key, key_value, &db.globals)?;
    }

    if let Some(device_keys) = &body.device_keys {
        // TODO: merge this and the existing event?
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

    Ok(upload_keys::v3::Response {
        one_time_key_counts: db.users.count_one_time_keys(sender_user, sender_device)?,
    })
}

/// # `POST /_matrix/client/r0/keys/query`
///
/// Get end-to-end encryption keys for the given users.
///
/// - Always fetches users from other servers over federation
/// - Gets master keys, self-signing keys, user signing keys and device keys.
/// - The master and self-signing keys contain signatures that the user is allowed to see
pub async fn get_keys_route(
    db: DatabaseGuard,
    body: Ruma<get_keys::v3::IncomingRequest>,
) -> Result<get_keys::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let response = get_keys_helper(
        Some(sender_user),
        &body.device_keys,
        |u| u == sender_user,
        &db,
    )
    .await?;

    Ok(response)
}

/// # `POST /_matrix/client/r0/keys/claim`
///
/// Claims one-time keys
pub async fn claim_keys_route(
    db: DatabaseGuard,
    body: Ruma<claim_keys::v3::Request>,
) -> Result<claim_keys::v3::Response> {
    let response = claim_keys_helper(&body.one_time_keys, &db).await?;

    db.flush()?;

    Ok(response)
}

/// # `POST /_matrix/client/r0/keys/device_signing/upload`
///
/// Uploads end-to-end key information for the sender user.
///
/// - Requires UIAA to verify password
pub async fn upload_signing_keys_route(
    db: DatabaseGuard,
    body: Ruma<upload_signing_keys::v3::IncomingRequest>,
) -> Result<upload_signing_keys::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_ref().expect("user is authenticated");

    // UIAA
    let mut uiaainfo = UiaaInfo {
        flows: vec![AuthFlow {
            stages: vec![AuthType::Password],
        }],
        completed: Vec::new(),
        params: Default::default(),
        session: None,
        auth_error: None,
    };

    if let Some(auth) = &body.auth {
        let (worked, uiaainfo) = db.uiaa.try_auth(
            sender_user,
            sender_device,
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
            .create(sender_user, sender_device, &uiaainfo, &json)?;
        return Err(Error::Uiaa(uiaainfo));
    } else {
        return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
    }

    if let Some(master_key) = &body.master_key {
        db.users.add_cross_signing_keys(
            sender_user,
            master_key,
            &body.self_signing_key,
            &body.user_signing_key,
            &db.rooms,
            &db.globals,
        )?;
    }

    db.flush()?;

    Ok(upload_signing_keys::v3::Response {})
}

/// # `POST /_matrix/client/r0/keys/signatures/upload`
///
/// Uploads end-to-end key signatures from the sender user.
pub async fn upload_signatures_route(
    db: DatabaseGuard,
    body: Ruma<upload_signatures::v3::Request>,
) -> Result<upload_signatures::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    for (user_id, signed_keys) in &body.signed_keys {
        for (key_id, signed_key) in signed_keys {
            let signed_key = serde_json::to_value(signed_key).unwrap();

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
                    user_id,
                    key_id,
                    signature,
                    sender_user,
                    &db.rooms,
                    &db.globals,
                )?;
            }
        }
    }

    db.flush()?;

    Ok(upload_signatures::v3::Response {
        failures: BTreeMap::new(), // TODO: integrate
    })
}

/// # `POST /_matrix/client/r0/keys/changes`
///
/// Gets a list of users who have updated their device identity keys since the previous sync token.
///
/// - TODO: left users
pub async fn get_key_changes_route(
    db: DatabaseGuard,
    body: Ruma<get_key_changes::v3::IncomingRequest>,
) -> Result<get_key_changes::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut device_list_updates = HashSet::new();

    device_list_updates.extend(
        db.users
            .keys_changed(
                sender_user.as_str(),
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
    Ok(get_key_changes::v3::Response {
        changed: device_list_updates.into_iter().collect(),
        left: Vec::new(), // TODO
    })
}

pub(crate) async fn get_keys_helper<F: Fn(&UserId) -> bool>(
    sender_user: Option<&UserId>,
    device_keys_input: &BTreeMap<Box<UserId>, Vec<Box<DeviceId>>>,
    allowed_signatures: F,
    db: &Database,
) -> Result<get_keys::v3::Response> {
    let mut master_keys = BTreeMap::new();
    let mut self_signing_keys = BTreeMap::new();
    let mut user_signing_keys = BTreeMap::new();
    let mut device_keys = BTreeMap::new();

    let mut get_over_federation = HashMap::new();

    for (user_id, device_ids) in device_keys_input {
        let user_id: &UserId = &**user_id;

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

                    add_unsigned_device_display_name(&mut keys, metadata)
                        .map_err(|_| Error::bad_database("invalid device keys in database"))?;
                    container.insert(device_id, keys);
                }
            }
            device_keys.insert(user_id.to_owned(), container);
        } else {
            for device_id in device_ids {
                let mut container = BTreeMap::new();
                if let Some(mut keys) = db.users.get_device_keys(user_id, device_id)? {
                    let metadata = db.users.get_device_metadata(user_id, device_id)?.ok_or(
                        Error::BadRequest(
                            ErrorKind::InvalidParam,
                            "Tried to get keys for nonexistent device.",
                        ),
                    )?;

                    add_unsigned_device_display_name(&mut keys, metadata)
                        .map_err(|_| Error::bad_database("invalid device keys in database"))?;
                    container.insert(device_id.to_owned(), keys);
                }
                device_keys.insert(user_id.to_owned(), container);
            }
        }

        if let Some(master_key) = db.users.get_master_key(user_id, &allowed_signatures)? {
            master_keys.insert(user_id.to_owned(), master_key);
        }
        if let Some(self_signing_key) = db
            .users
            .get_self_signing_key(user_id, &allowed_signatures)?
        {
            self_signing_keys.insert(user_id.to_owned(), self_signing_key);
        }
        if Some(user_id) == sender_user {
            if let Some(user_signing_key) = db.users.get_user_signing_key(user_id)? {
                user_signing_keys.insert(user_id.to_owned(), user_signing_key);
            }
        }
    }

    let mut failures = BTreeMap::new();

    let mut futures: FuturesUnordered<_> = get_over_federation
        .into_iter()
        .map(|(server, vec)| async move {
            let mut device_keys_input_fed = BTreeMap::new();
            for (user_id, keys) in vec {
                device_keys_input_fed.insert(user_id.to_owned(), keys.clone());
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
        .collect();

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

    Ok(get_keys::v3::Response {
        master_keys,
        self_signing_keys,
        user_signing_keys,
        device_keys,
        failures,
    })
}

fn add_unsigned_device_display_name(
    keys: &mut Raw<ruma::encryption::DeviceKeys>,
    metadata: ruma::api::client::device::Device,
) -> serde_json::Result<()> {
    if let Some(display_name) = metadata.display_name {
        let mut object = keys.deserialize_as::<serde_json::Map<String, serde_json::Value>>()?;

        let unsigned = object.entry("unsigned").or_insert_with(|| json!({}));
        if let serde_json::Value::Object(unsigned_object) = unsigned {
            unsigned_object.insert("device_display_name".to_owned(), display_name.into());
        }

        *keys = Raw::from_json(serde_json::value::to_raw_value(&object)?);
    }

    Ok(())
}

pub(crate) async fn claim_keys_helper(
    one_time_keys_input: &BTreeMap<Box<UserId>, BTreeMap<Box<DeviceId>, DeviceKeyAlgorithm>>,
    db: &Database,
) -> Result<claim_keys::v3::Response> {
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

    Ok(claim_keys::v3::Response {
        failures,
        one_time_keys,
    })
}
