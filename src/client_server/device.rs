use crate::{database::DatabaseGuard, utils, ConduitResult, Error, Ruma};
use ruma::api::client::{
    error::ErrorKind,
    r0::{
        device::{self, delete_device, delete_devices, get_device, get_devices, update_device},
        uiaa::{AuthFlow, UiaaInfo},
    },
};

use super::SESSION_ID_LENGTH;
#[cfg(feature = "conduit_bin")]
use rocket::{delete, get, post, put};

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/devices", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_devices_route(
    db: DatabaseGuard,
    body: Ruma<get_devices::Request>,
) -> ConduitResult<get_devices::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let devices = db
        .users
        .all_devices_metadata(sender_user)
        .filter_map(|r| r.ok()) // Filter out buggy devices
        .collect::<Vec<device::Device>>();

    Ok(get_devices::Response { devices }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/client/r0/devices/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn get_device_route(
    db: DatabaseGuard,
    body: Ruma<get_device::Request<'_>>,
) -> ConduitResult<get_device::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let device = db
        .users
        .get_device_metadata(&sender_user, &body.body.device_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Device not found."))?;

    Ok(get_device::Response { device }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/devices/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn update_device_route(
    db: DatabaseGuard,
    body: Ruma<update_device::Request<'_>>,
) -> ConduitResult<update_device::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut device = db
        .users
        .get_device_metadata(&sender_user, &body.device_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Device not found."))?;

    device.display_name = body.display_name.clone();

    db.users
        .update_device_metadata(&sender_user, &body.device_id, &device)?;

    db.flush().await?;

    Ok(update_device::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    delete("/_matrix/client/r0/devices/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn delete_device_route(
    db: DatabaseGuard,
    body: Ruma<delete_device::Request<'_>>,
) -> ConduitResult<delete_device::Response> {
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
    } else {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa.create(
            &sender_user,
            &sender_device,
            &uiaainfo,
            &body.json_body.expect("body is json"),
        )?;
        return Err(Error::Uiaa(uiaainfo));
    }

    db.users.remove_device(&sender_user, &body.device_id)?;

    db.flush().await?;

    Ok(delete_device::Response.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/client/r0/delete_devices", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn delete_devices_route(
    db: DatabaseGuard,
    body: Ruma<delete_devices::Request<'_>>,
) -> ConduitResult<delete_devices::Response> {
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
    } else {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        db.uiaa.create(
            &sender_user,
            &sender_device,
            &uiaainfo,
            &body.json_body.expect("body is json"),
        )?;
        return Err(Error::Uiaa(uiaainfo));
    }

    for device_id in &body.devices {
        db.users.remove_device(&sender_user, &device_id)?
    }

    db.flush().await?;

    Ok(delete_devices::Response.into())
}
