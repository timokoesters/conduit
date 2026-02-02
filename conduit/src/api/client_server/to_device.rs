use std::collections::BTreeMap;

use crate::{services, Error, Result, Ruma};
use ruma::{
    api::{
        client::{error::ErrorKind, to_device::send_event_to_device},
        federation::{self, transactions::edu::DirectDeviceContent},
    },
    to_device::DeviceIdOrAllDevices,
};

/// # `PUT /_matrix/client/r0/sendToDevice/{eventType}/{txnId}`
///
/// Send a to-device event to a set of client devices.
pub async fn send_event_to_device_route(
    body: Ruma<send_event_to_device::v3::Request>,
) -> Result<send_event_to_device::v3::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_deref();

    // Check if this is a new transaction id
    if services()
        .transaction_ids
        .existing_txnid(sender_user, sender_device, &body.txn_id)?
        .is_some()
    {
        return Ok(send_event_to_device::v3::Response {});
    }

    for (target_user_id, map) in &body.messages {
        for (target_device_id_maybe, event) in map {
            if target_user_id.server_name() != services().globals.server_name() {
                let mut map = BTreeMap::new();
                map.insert(target_device_id_maybe.clone(), event.clone());
                let mut messages = BTreeMap::new();
                messages.insert(target_user_id.clone(), map);
                let count = services().globals.next_count()?;

                services().sending.send_reliable_edu(
                    target_user_id.server_name(),
                    serde_json::to_vec(&federation::transactions::edu::Edu::DirectToDevice(
                        DirectDeviceContent {
                            sender: sender_user.clone(),
                            ev_type: body.event_type.clone(),
                            message_id: count.to_string().into(),
                            messages,
                        },
                    ))
                    .expect("DirectToDevice EDU can be serialized"),
                    count,
                )?;

                continue;
            }

            match target_device_id_maybe {
                DeviceIdOrAllDevices::DeviceId(target_device_id) => {
                    services().users.add_to_device_event(
                        sender_user,
                        target_user_id,
                        target_device_id,
                        &body.event_type.to_string(),
                        event.deserialize_as().map_err(|_| {
                            Error::BadRequest(ErrorKind::InvalidParam, "Event is invalid")
                        })?,
                    )?
                }

                DeviceIdOrAllDevices::AllDevices => {
                    for target_device_id in services().users.all_device_ids(target_user_id) {
                        services().users.add_to_device_event(
                            sender_user,
                            target_user_id,
                            &target_device_id?,
                            &body.event_type.to_string(),
                            event.deserialize_as().map_err(|_| {
                                Error::BadRequest(ErrorKind::InvalidParam, "Event is invalid")
                            })?,
                        )?;
                    }
                }
            }
        }
    }

    // Save transaction id with empty data
    services()
        .transaction_ids
        .add_txnid(sender_user, sender_device, &body.txn_id, &[])?;

    Ok(send_event_to_device::v3::Response {})
}
