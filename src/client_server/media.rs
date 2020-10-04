use super::State;
use crate::{
    database::media::FileMeta, server_server, utils, ConduitResult, Database, Error, Ruma,
};
use ruma::api::client::{
    error::ErrorKind,
    r0::media::{create_content, get_content, get_content_thumbnail, get_media_config},
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};
use std::convert::TryInto;

const MXC_LENGTH: usize = 32;

#[cfg_attr(feature = "conduit_bin", get("/_matrix/media/r0/config"))]
pub fn get_media_config_route(
    db: State<'_, Database<'_>>,
) -> ConduitResult<get_media_config::Response> {
    Ok(get_media_config::Response {
        upload_size: db.globals.max_request_size().into(),
    }
    .into())
}

#[cfg_attr(
    feature = "conduit_bin",
    post("/_matrix/media/r0/upload", data = "<body>")
)]
pub fn create_content_route(
    db: State<'_, Database<'_>>,
    body: Ruma<create_content::Request<'_>>,
) -> ConduitResult<create_content::Response> {
    let mxc = format!(
        "mxc://{}/{}",
        db.globals.server_name(),
        utils::random_string(MXC_LENGTH)
    );
    db.media.create(
        mxc.clone(),
        &body.filename.as_deref(),
        &body.content_type,
        &body.file,
    )?;

    Ok(create_content::Response { content_uri: mxc }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/media/r0/download/<_>/<_>", data = "<body>")
)]
pub async fn get_content_route(
    db: State<'_, Database<'_>>,
    body: Ruma<get_content::Request<'_>>,
) -> ConduitResult<get_content::Response> {
    let mxc = format!("mxc://{}/{}", body.server_name, body.media_id);

    if let Some(FileMeta {
        filename,
        content_type,
        file,
    }) = db.media.get(&mxc)?
    {
        Ok(get_content::Response {
            file,
            content_type,
            content_disposition: filename.unwrap_or_default(), // TODO: Spec says this should be optional
        }
        .into())
    } else if &*body.server_name != db.globals.server_name() && body.allow_remote {
        let get_content_response = server_server::send_request(
            &db.globals,
            body.server_name.clone(),
            get_content::Request {
                allow_remote: false,
                server_name: &body.server_name,
                media_id: &body.media_id,
            },
        )
        .await?;

        db.media.create(
            mxc,
            &Some(&get_content_response.content_disposition),
            &get_content_response.content_type,
            &get_content_response.file,
        )?;

        Ok(get_content_response.into())
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}

#[cfg_attr(
    feature = "conduit_bin",
    get("/_matrix/media/r0/thumbnail/<_>/<_>", data = "<body>")
)]
pub async fn get_content_thumbnail_route(
    db: State<'_, Database<'_>>,
    body: Ruma<get_content_thumbnail::Request<'_>>,
) -> ConduitResult<get_content_thumbnail::Response> {
    let mxc = format!("mxc://{}/{}", body.server_name, body.media_id);

    if let Some(FileMeta {
        content_type, file, ..
    }) = db.media.get_thumbnail(
        mxc.clone(),
        body.width
            .try_into()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
        body.height
            .try_into()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
    )? {
        Ok(get_content_thumbnail::Response { file, content_type }.into())
    } else if &*body.server_name != db.globals.server_name() && body.allow_remote {
        let get_thumbnail_response = server_server::send_request(
            &db.globals,
            body.server_name.clone(),
            get_content_thumbnail::Request {
                allow_remote: false,
                height: body.height,
                width: body.width,
                method: body.method,
                server_name: &body.server_name,
                media_id: &body.media_id,
            },
        )
        .await?;

        db.media.upload_thumbnail(
            mxc,
            &None,
            &get_thumbnail_response.content_type,
            body.width.try_into().expect("all UInts are valid u32s"),
            body.height.try_into().expect("all UInts are valid u32s"),
            &get_thumbnail_response.file,
        )?;

        Ok(get_thumbnail_response.into())
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}
