use std::time::Instant;

use ruma::api::{
    appservice::ping::send_ping,
    client::{appservice::request_ping, error::ErrorKind},
};

use crate::{api::appservice_server, Error, Result, Ruma};

/// # `POST /_matrix/client/v1/appservice/{appserviceId}/ping`
///
/// Allows an appservice to check whether the server and
/// appservice can connect, and how fast their connection is
pub async fn ping_appservice_route(
    body: Ruma<request_ping::v1::Request>,
) -> Result<request_ping::v1::Response> {
    let Ruma::<request_ping::v1::Request> {
        appservice_info,
        body,
        ..
    } = body;

    let registration = appservice_info
        .expect("Only appservices can call this endpoint")
        .registration;

    if registration.id != body.appservice_id {
        return Err(Error::BadRequest(
            ErrorKind::forbidden(),
            "Appservice ID specified in path does not match the requesting access token",
        ));
    }

    if registration.url.is_some() {
        let start = Instant::now();
        let response = appservice_server::send_request(
            registration,
            send_ping::v1::Request {
                transaction_id: body.transaction_id,
            },
        )
        .await;
        let elapsed = start.elapsed();

        if let Err(error) = response {
            Err(match error {
                Error::ReqwestError { source } => {
                    if source.is_timeout() {
                        Error::BadRequest(
                            ErrorKind::ConnectionTimeout,
                            "Connection to appservice timed-out",
                        )
                    } else if let Some(status_code) = source.status() {
                        Error::BadRequest(
                            ErrorKind::BadStatus {
                                status: Some(status_code),
                                body: Some(source.to_string()),
                            },
                            "Ping returned error status",
                        )
                    } else {
                        Error::BadRequest(ErrorKind::ConnectionFailed, "Failed to ping appservice")
                    }
                }
                Error::BadServerResponse(_) => Error::BadRequest(
                    ErrorKind::ConnectionFailed,
                    "Received invalid response from appservice",
                ),
                e => e,
            })
        } else {
            Ok(request_ping::v1::Response::new(elapsed))
        }
    } else {
        Err(Error::BadRequest(
            ErrorKind::UrlNotSet,
            "Appservice doesn't have a URL configured",
        ))
    }
}
