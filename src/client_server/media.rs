use super::State;
use crate::{database::media::FileMeta, utils, ConduitResult, Database, Error, Ruma};
use ruma::api::client::{
    error::ErrorKind,
    r0::media::{create_content, get_content, get_content_thumbnail, get_media_config},
};

#[cfg(feature = "conduit_bin")]
use rocket::{get, post};
use std::convert::TryInto;

const MXC_LENGTH: usize = 256;

#[cfg_attr(feature = "conduit_bin", get("/_matrix/media/r0/config"))]
pub fn get_media_config_route(
    db: State<'_, Database>,
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
    db: State<'_, Database>,
    body: Ruma<create_content::Request>,
) -> ConduitResult<create_content::Response> {
    let mxc = format!(
        "mxc://{}/{}",
        db.globals.server_name(),
        utils::random_string(MXC_LENGTH)
    );
    db.media.create(
        mxc.clone(),
        body.filename.as_ref(),
        &body.content_type,
        &body.file,
    )?;

    Ok(create_content::Response { content_uri: mxc }.into())
}

#[cfg_attr(
    feature = "conduit_bin",
    get(
        "/_matrix/media/r0/download/<_server_name>/<_media_id>",
        data = "<body>"
    )
)]
pub fn get_content_route(
    db: State<'_, Database>,
    body: Ruma<get_content::IncomingRequest>,
    _server_name: String,
    _media_id: String,
) -> ConduitResult<get_content::Response> {
    if let Some(FileMeta {
        filename,
        content_type,
        file,
    }) = db
        .media
        .get(format!("mxc://{}/{}", body.server_name, body.media_id))?
    {
        Ok(get_content::Response {
            file,
            content_type,
            content_disposition: filename.unwrap_or_default(), // TODO: Spec says this should be optional
        }
        .into())
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}

#[cfg_attr(
    feature = "conduit_bin",
    get(
        "/_matrix/media/r0/thumbnail/<_server_name>/<_media_id>",
        data = "<body>"
    )
)]
pub fn get_content_thumbnail_route(
    db: State<'_, Database>,
    body: Ruma<get_content_thumbnail::IncomingRequest>,
    _server_name: String,
    _media_id: String,
) -> ConduitResult<get_content_thumbnail::Response> {
    if let Some(FileMeta {
        content_type, file, ..
    }) = db.media.get_thumbnail(
        format!("mxc://{}/{}", body.server_name, body.media_id),
        body.width
            .try_into()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
        body.height
            .try_into()
            .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
    )? {
        Ok(get_content_thumbnail::Response { file, content_type }.into())
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}
