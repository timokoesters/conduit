use crate::{database::DatabaseGuard, ConduitResult, Error, Ruma};
use ruma::{
    api::client::{error::ErrorKind, r0::to_device::send_event_to_device},
    to_device::DeviceIdOrAllDevices,
};

#[cfg(feature = "conduit_bin")]
use rocket::put;

#[cfg_attr(
    feature = "conduit_bin",
    put("/_matrix/client/r0/sendToDevice/<_>/<_>", data = "<body>")
)]
#[tracing::instrument(skip(db, body))]
pub async fn send_event_to_device_route(
    db: DatabaseGuard,
    body: Ruma<send_event_to_device::Request<'_>>,
) -> ConduitResult<send_event_to_device::Response> {
    let sender_user = body.sender_user.as_ref().expect("user is authenticated");
    let sender_device = body.sender_device.as_deref();

    // TODO: uncomment when https://github.com/vector-im/element-android/issues/3589 is solved
    // Check if this is a new transaction id
    /*
    if db
        .transaction_ids
        .existing_txnid(sender_user, sender_device, &body.txn_id)?
        .is_some()
    {
        return Ok(send_event_to_device::Response.into());
    }
    */

    for (target_user_id, map) in &body.messages {
        for (target_device_id_maybe, event) in map {
            match target_device_id_maybe {
                DeviceIdOrAllDevices::DeviceId(target_device_id) => db.users.add_to_device_event(
                    sender_user,
                    &target_user_id,
                    &target_device_id,
                    &body.event_type,
                    event.deserialize_as().map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidParam, "Event is invalid")
                    })?,
                    &db.globals,
                )?,

                DeviceIdOrAllDevices::AllDevices => {
                    for target_device_id in db.users.all_device_ids(&target_user_id) {
                        db.users.add_to_device_event(
                            sender_user,
                            &target_user_id,
                            &target_device_id?,
                            &body.event_type,
                            event.deserialize_as().map_err(|_| {
                                Error::BadRequest(ErrorKind::InvalidParam, "Event is invalid")
                            })?,
                            &db.globals,
                        )?;
                    }
                }
            }
        }
    }

    // Save transaction id with empty data
    db.transaction_ids
        .add_txnid(sender_user, sender_device, &body.txn_id, &[])?;

    db.flush().await?;

    Ok(send_event_to_device::Response.into())
}
