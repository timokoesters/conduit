impl service::uiaa::Data for KeyValueDatabase {
    fn set_uiaa_request(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
        request: &CanonicalJsonValue,
    ) -> Result<()> {
        self.userdevicesessionid_uiaarequest
            .write()
            .unwrap()
            .insert(
                (user_id.to_owned(), device_id.to_owned(), session.to_owned()),
                request.to_owned(),
            );

        Ok(())
    }

    fn get_uiaa_request(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
    ) -> Option<CanonicalJsonValue> {
        self.userdevicesessionid_uiaarequest
            .read()
            .unwrap()
            .get(&(user_id.to_owned(), device_id.to_owned(), session.to_owned()))
            .map(|j| j.to_owned())
    }

    fn update_uiaa_session(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
        uiaainfo: Option<&UiaaInfo>,
    ) -> Result<()> {
        let mut userdevicesessionid = user_id.as_bytes().to_vec();
        userdevicesessionid.push(0xff);
        userdevicesessionid.extend_from_slice(device_id.as_bytes());
        userdevicesessionid.push(0xff);
        userdevicesessionid.extend_from_slice(session.as_bytes());

        if let Some(uiaainfo) = uiaainfo {
            self.userdevicesessionid_uiaainfo.insert(
                &userdevicesessionid,
                &serde_json::to_vec(&uiaainfo).expect("UiaaInfo::to_vec always works"),
            )?;
        } else {
            self.userdevicesessionid_uiaainfo
                .remove(&userdevicesessionid)?;
        }

        Ok(())
    }

    fn get_uiaa_session(
        &self,
        user_id: &UserId,
        device_id: &DeviceId,
        session: &str,
    ) -> Result<UiaaInfo> {
        let mut userdevicesessionid = user_id.as_bytes().to_vec();
        userdevicesessionid.push(0xff);
        userdevicesessionid.extend_from_slice(device_id.as_bytes());
        userdevicesessionid.push(0xff);
        userdevicesessionid.extend_from_slice(session.as_bytes());

        serde_json::from_slice(
            &self
                .userdevicesessionid_uiaainfo
                .get(&userdevicesessionid)?
                .ok_or(Error::BadRequest(
                    ErrorKind::Forbidden,
                    "UIAA session does not exist.",
                ))?,
        )
        .map_err(|_| Error::bad_database("UiaaInfo in userdeviceid_uiaainfo is invalid."))
    }
}
