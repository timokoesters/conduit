use crate::{Error, Result, Ruma, services, utils::HtmlEscape};
use ruma::{
    api::{client::room::report_content, error::ErrorKind},
    events::room::message,
};

/// # `POST /_matrix/client/r0/rooms/{roomId}/report/{eventId}`
///
/// Reports an inappropriate event to homeserver admins
///
#[conduit_macros::rate_limiting(ClientRestriction::SendReport)]
pub async fn report_event_route(
    body: Ruma<report_content::v3::Request>,
) -> Result<report_content::v3::Response> {
    let pdu = match services().rooms.timeline.get_pdu(&body.event_id)? {
        Some(pdu) => pdu,
        _ => {
            return Err(Error::BadRequest(
                ErrorKind::InvalidParam,
                "Invalid Event ID",
            ));
        }
    };

    if let Some(true) = body.reason.clone().map(|s| s.chars().count() > 250) {
        return Err(Error::BadRequest(
            ErrorKind::InvalidParam,
            "Reason too long, should be 250 characters or fewer",
        ));
    };

    services().admin
        .send_message(message::RoomMessageEventContent::text_html(
            format!(
                "Report received from: {}\n\n\
                Event ID: {:?}\n\
                Room ID: {:?}\n\
                Sent By: {:?}\n\n\
                Report Reason: {:?}",
                &body.sender.user_id, pdu.event_id, pdu.room_id, pdu.sender, body.reason
            ),
            format!(
                "<details><summary>Report received from: <a href=\"https://matrix.to/#/{0:?}\">{0:?}\
                </a></summary><ul><li>Event Info<ul><li>Event ID: <code>{1:?}</code>\
                <a href=\"https://matrix.to/#/{2:?}/{1:?}\">🔗</a></li><li>Room ID: <code>{2:?}</code>\
                </li><li>Sent By: <a href=\"https://matrix.to/#/{3:?}\">{3:?}</a></li></ul></li><li>\
                Report Info<ul><li>Report Reason: {4}</li></ul></li>\
                </ul></details>",
                &body.sender.user_id,
                pdu.event_id,
                pdu.room_id,
                pdu.sender,
                HtmlEscape(body.reason.as_deref().unwrap_or(""))
            ),
        ));

    Ok(report_content::v3::Response {})
}
