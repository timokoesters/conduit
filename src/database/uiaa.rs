use crate::{Error, Result};
use ruma::{
    api::client::{
        error::ErrorKind,
        r0::uiaa::{IncomingAuthData, UiaaInfo},
    },
    DeviceId, UserId,
};

pub struct Uiaa {
    pub(super) userdeviceid_uiaainfo: sled::Tree, // User-interactive authentication
}

impl Uiaa {
    /// Creates a new Uiaa session. Make sure the session token is unique.
    pub fn create(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        uiaainfo: &UiaaInfo,
    ) -> Result<()> {
        self.update_uiaa_session(user_id, device_id, Some(uiaainfo))
    }

    pub fn try_auth(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        auth: &IncomingAuthData,
        uiaainfo: &UiaaInfo,
        users: &super::users::Users,
        globals: &super::globals::Globals<'_>,
    ) -> Result<(bool, UiaaInfo)> {
        if let IncomingAuthData::DirectRequest {
            kind,
            session,
            auth_parameters,
        } = &auth
        {
            let mut uiaainfo = session
                .as_ref()
                .map(|session| {
                    Ok::<_, Error>(self.get_uiaa_session(&user_id, &device_id, session)?)
                })
                .unwrap_or_else(|| Ok(uiaainfo.clone()))?;

            // Find out what the user completed
            match &**kind {
                "m.login.password" => {
                    let identifier = auth_parameters.get("identifier").ok_or(Error::BadRequest(
                        ErrorKind::MissingParam,
                        "m.login.password needs identifier.",
                    ))?;

                    let identifier_type = identifier.get("type").ok_or(Error::BadRequest(
                        ErrorKind::MissingParam,
                        "Identifier needs a type.",
                    ))?;

                    if identifier_type != "m.id.user" {
                        return Err(Error::BadRequest(
                            ErrorKind::Unrecognized,
                            "Identifier type not recognized.",
                        ));
                    }

                    let username = identifier
                        .get("user")
                        .ok_or(Error::BadRequest(
                            ErrorKind::MissingParam,
                            "Identifier needs user field.",
                        ))?
                        .as_str()
                        .ok_or(Error::BadRequest(
                            ErrorKind::BadJson,
                            "User is not a string.",
                        ))?;

                    let user_id = UserId::parse_with_server_name(username, globals.server_name())
                        .map_err(|_| {
                        Error::BadRequest(ErrorKind::InvalidParam, "User ID is invalid.")
                    })?;

                    let password = auth_parameters
                        .get("password")
                        .ok_or(Error::BadRequest(
                            ErrorKind::MissingParam,
                            "Password is missing.",
                        ))?
                        .as_str()
                        .ok_or(Error::BadRequest(
                            ErrorKind::BadJson,
                            "Password is not a string.",
                        ))?;

                    // Check if password is correct
                    if let Some(hash) = users.password_hash(&user_id)? {
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
                    uiaainfo.completed.push("m.login.password".to_owned());
                }
                "m.login.dummy" => {
                    uiaainfo.completed.push("m.login.dummy".to_owned());
                }
                k => panic!("type not supported: {}", k),
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
                self.update_uiaa_session(user_id, device_id, Some(&uiaainfo))?;
                return Ok((false, uiaainfo));
            }

            // UIAA was successful! Remove this session and return true
            self.update_uiaa_session(user_id, device_id, None)?;
            Ok((true, uiaainfo))
        } else {
            panic!("FallbackAcknowledgement is not supported yet");
        }
    }

    fn update_uiaa_session(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        uiaainfo: Option<&UiaaInfo>,
    ) -> Result<()> {
        let mut userdeviceid = user_id.to_string().as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        if let Some(uiaainfo) = uiaainfo {
            self.userdeviceid_uiaainfo.insert(
                &userdeviceid,
                &*serde_json::to_string(&uiaainfo).expect("UiaaInfo::to_string always works"),
            )?;
        } else {
            self.userdeviceid_uiaainfo.remove(&userdeviceid)?;
        }

        Ok(())
    }

    fn get_uiaa_session(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
    ) -> Result<UiaaInfo> {
        let mut userdeviceid = user_id.to_string().as_bytes().to_vec();
        userdeviceid.push(0xff);
        userdeviceid.extend_from_slice(device_id.as_bytes());

        let uiaainfo = serde_json::from_slice::<UiaaInfo>(
            &self
                .userdeviceid_uiaainfo
                .get(&userdeviceid)?
                .ok_or(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "UIAA session does not exist.",
                ))?,
        )
        .map_err(|_| Error::bad_database("UiaaInfo in userdeviceid_uiaainfo is invalid."))?;

        if uiaainfo
            .session
            .as_ref()
            .filter(|&s| s == session)
            .is_none()
        {
            return Err(Error::BadRequest(
                ErrorKind::Forbidden,
                "UIAA session token invalid.",
            ));
        }

        Ok(uiaainfo)
    }
}
