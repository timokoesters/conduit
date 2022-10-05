mod data;
use std::sync::Arc;

pub use data::Data;

use ruma::{api::client::{uiaa::{UiaaInfo, IncomingAuthData, IncomingPassword, AuthType, IncomingUserIdentifier}, error::ErrorKind}, DeviceId, UserId, signatures::CanonicalJsonValue};
use tracing::error;

use crate::{Result, utils, Error, services, api::client_server::SESSION_ID_LENGTH};

pub struct Service {
    db: Arc<dyn Data>,
}

impl Service {
    /// Creates a new Uiaa session. Make sure the session token is unique.
    pub fn create(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        uiaainfo: &UiaaInfo,
        json_body: &CanonicalJsonValue,
    ) -> Result<()> {
        self.db.set_uiaa_request(
            user_id,
            device_id,
            uiaainfo.session.as_ref().expect("session should be set"), // TODO: better session error handling (why is it optional in ruma?)
            json_body,
        )?;
        self.db.update_uiaa_session(
            user_id,
            device_id,
            uiaainfo.session.as_ref().expect("session should be set"),
            Some(uiaainfo),
        )
    }

    pub fn try_auth(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        auth: &IncomingAuthData,
        uiaainfo: &UiaaInfo,
    ) -> Result<(bool, UiaaInfo)> {
        let mut uiaainfo = auth
            .session()
            .map(|session| self.db.get_uiaa_session(user_id, device_id, session))
            .unwrap_or_else(|| Ok(uiaainfo.clone()))?;

        if uiaainfo.session.is_none() {
            uiaainfo.session = Some(utils::random_string(SESSION_ID_LENGTH));
        }

        match auth {
            // Find out what the user completed
            IncomingAuthData::Password(IncomingPassword {
                identifier,
                password,
                ..
            }) => {
                let username = match identifier {
                    IncomingUserIdentifier::UserIdOrLocalpart(username) => username,
                    _ => {
                        return Err(Error::BadRequest(
                            ErrorKind::Unrecognized,
                            "Identifier type not recognized.",
                        ))
                    }
                };

                let user_id =
                    UserId::parse_with_server_name(username.clone(), services().globals.server_name())
                        .map_err(|_| {
                            Error::BadRequest(ErrorKind::InvalidParam, "User ID is invalid.")
                        })?;

                // Check if password is correct
                if let Some(hash) = services().users.password_hash(&user_id)? {
                    let hash_matches =
                        argon2::verify_encoded(&hash, password.as_bytes()).unwrap_or(false);

                    if !hash_matches {
                        uiaainfo.auth_error = Some(ruma::api::client::error::ErrorBody {
                            kind: ErrorKind::Forbidden,
                            message: "Invalid username or password.".to_owned(),
                        });
                        return Ok((false, uiaainfo));
                    }
                }

                // Password was correct! Let's add it to `completed`
                uiaainfo.completed.push(AuthType::Password);
            }
            IncomingAuthData::Dummy(_) => {
                uiaainfo.completed.push(AuthType::Dummy);
            }
            k => error!("type not supported: {:?}", k),
        }

        // Check if a flow now succeeds
        let mut completed = false;
        'flows: for flow in &mut uiaainfo.flows {
            for stage in &flow.stages {
                if !uiaainfo.completed.contains(stage) {
                    continue 'flows;
                }
            }
            // We didn't break, so this flow succeeded!
            completed = true;
        }

        if !completed {
            self.db.update_uiaa_session(
                user_id,
                device_id,
                uiaainfo.session.as_ref().expect("session is always set"),
                Some(&uiaainfo),
            )?;
            return Ok((false, uiaainfo));
        }

        // UIAA was successful! Remove this session and return true
        self.db.update_uiaa_session(
            user_id,
            device_id,
            uiaainfo.session.as_ref().expect("session is always set"),
            None,
        )?;
        Ok((true, uiaainfo))
    }

    pub fn get_uiaa_request(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
    ) -> Option<CanonicalJsonValue> {
        self.db.get_uiaa_request(user_id, device_id, session)
    }
}
