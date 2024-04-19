use crate::{services, utils, Error, Result, Ruma};
use ruma::api::client::{
    device::{self, delete_device, delete_devices, get_device, get_devices, update_device},
    error::ErrorKind,
    uiaa::{AuthFlow, AuthType, UiaaInfo},
};

use super::SESSION_ID_LENGTH;

/// # `GET /_matrix/client/r0/devices`
///
/// Get metadata on all devices of the sender user.
pub async fn get_devices_route(
    body: Ruma<get_devices::v3::Request>,
) -> Result<get_devices::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let devices: Vec<device::Device> = services()
        .users
        .all_user_devices_metadata(sender_user)
        .await
        .collect();

    Ok(get_devices::v3::Response { devices })
}

/// # `GET /_matrix/client/r0/devices/{deviceId}`
///
/// Get metadata on a single device of the sender user.
pub async fn get_device_route(
    body: Ruma<get_device::v3::Request>,
) -> Result<get_device::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let device = services()
        .users
        .get_device_metadata(sender_user, &body.body.device_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Device not found."))?;

    Ok(get_device::v3::Response { device })
}

/// # `PUT /_matrix/client/r0/devices/{deviceId}`
///
/// Updates the metadata on a given device of the sender user.
pub async fn update_device_route(
    body: Ruma<update_device::v3::Request>,
) -> Result<update_device::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");

    let mut device = services()
        .users
        .get_device_metadata(sender_user, &body.device_id)?
        .ok_or(Error::BadRequest(ErrorKind::NotFound, "Device not found."))?;

    device.display_name.clone_from(&body.display_name);

    services()
        .users
        .update_device_metadata(sender_user, &body.device_id, &device)?;

    Ok(update_device::v3::Response {})
}

/// # `DELETE /_matrix/client/r0/devices/{deviceId}`
///
/// Deletes the given device.
///
/// - Requires UIAA to verify user password
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip, last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
pub async fn delete_device_route(
    body: Ruma<delete_device::v3::Request>,
) -> Result<delete_device::v3::Response> {
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
        let (worked, uiaainfo) =
            services()
                .uiaa
                .try_auth(sender_user, sender_device, auth, &uiaainfo)?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else if let Some(json) = body.json_body {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        services()
            .uiaa
            .create(sender_user, sender_device, &uiaainfo, &json)?;
        return Err(Error::Uiaa(uiaainfo));
    } else {
        return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
    }

    services()
        .users
        .remove_device(sender_user, &body.device_id)?;

    Ok(delete_device::v3::Response {})
}

/// # `PUT /_matrix/client/r0/devices/{deviceId}`
///
/// Deletes the given device.
///
/// - Requires UIAA to verify user password
///
/// For each device:
/// - Invalidates access token
/// - Deletes device metadata (device id, device display name, last seen ip, last seen ts)
/// - Forgets to-device events
/// - Triggers device list updates
pub async fn delete_devices_route(
    body: Ruma<delete_devices::v3::Request>,
) -> Result<delete_devices::v3::Response> {
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
        let (worked, uiaainfo) =
            services()
                .uiaa
                .try_auth(sender_user, sender_device, auth, &uiaainfo)?;
        if !worked {
            return Err(Error::Uiaa(uiaainfo));
        }
    // Success!
    } else if let Some(json) = body.json_body {
        uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        services()
            .uiaa
            .create(sender_user, sender_device, &uiaainfo, &json)?;
        return Err(Error::Uiaa(uiaainfo));
    } else {
        return Err(Error::BadRequest(ErrorKind::NotJson, "Not json."));
    }

    for device_id in &body.devices {
        services().users.remove_device(sender_user, device_id)?
    }

    Ok(delete_devices::v3::Response {})
}
