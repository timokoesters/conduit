use crate::{
    database::{media::FileMeta, DatabaseGuard},
    utils, Error, Result, Ruma,
};
use ruma::api::client::{
    error::ErrorKind,
    media::{
        create_content, get_content, get_content_as_filename, get_content_thumbnail,
        get_media_config,
    },
};

const MXC_LENGTH: usize = 32;

/// # `GET /_matrix/media/r0/config`
///
/// Returns max upload size.
pub async fn get_media_config_route(
    db: DatabaseGuard,
    _body: Ruma<get_media_config::v3::Request>,
) -> Result<get_media_config::v3::Response> {
    Ok(get_media_config::v3::Response {
        upload_size: db.globals.max_request_size().into(),
    })
}

/// # `POST /_matrix/media/r0/upload`
///
/// Permanently save media in the server.
///
/// - Some metadata will be saved in the database
/// - Media will be saved in the media/ directory
pub async fn create_content_route(
    db: DatabaseGuard,
    body: Ruma<create_content::v3::IncomingRequest>,
) -> Result<create_content::v3::Response> {
    let mxc = format!(
        "mxc://{}/{}",
        db.globals.server_name(),
        utils::random_string(MXC_LENGTH)
    );

    db.media
        .create(
            mxc.clone(),
            &db.globals,
            &body
                .filename
                .as_ref()
                .map(|filename| "inline; filename=".to_owned() + filename)
                .as_deref(),
            &body.content_type.as_deref(),
            &body.file,
        )
        .await?;

    db.flush()?;

    Ok(create_content::v3::Response {
        content_uri: mxc.try_into().expect("Invalid mxc:// URI"),
        blurhash: None,
    })
}

pub async fn get_remote_content(
    db: &DatabaseGuard,
    mxc: &str,
    server_name: &ruma::ServerName,
    media_id: &str,
) -> Result<get_content::v3::Response, Error> {
    let content_response = db
        .sending
        .send_federation_request(
            &db.globals,
            server_name,
            get_content::v3::Request {
                allow_remote: false,
                server_name,
                media_id,
            },
        )
        .await?;

    db.media
        .create(
            mxc.to_string(),
            &db.globals,
            &content_response.content_disposition.as_deref(),
            &content_response.content_type.as_deref(),
            &content_response.file,
        )
        .await?;

    Ok(content_response)
}

/// # `GET /_matrix/media/r0/download/{serverName}/{mediaId}`
///
/// Load media from our server or over federation.
///
/// - Only allows federation if `allow_remote` is true
pub async fn get_content_route(
    db: DatabaseGuard,
    body: Ruma<get_content::v3::IncomingRequest>,
) -> Result<get_content::v3::Response> {
    let mxc = format!("mxc://{}/{}", body.server_name, body.media_id);

    if let Some(FileMeta {
        content_disposition,
        content_type,
        file,
    }) = db.media.get(&db.globals, &mxc).await?
    {
        Ok(get_content::v3::Response {
            file,
            content_type,
            content_disposition,
        })
    } else if &*body.server_name != db.globals.server_name() && body.allow_remote {
        let remote_content_response =
            get_remote_content(&db, &mxc, &body.server_name, &body.media_id).await?;
        Ok(remote_content_response)
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}

/// # `GET /_matrix/media/r0/download/{serverName}/{mediaId}/{fileName}`
///
/// Load media from our server or over federation, permitting desired filename.
///
/// - Only allows federation if `allow_remote` is true
pub async fn get_content_as_filename_route(
    db: DatabaseGuard,
    body: Ruma<get_content_as_filename::v3::IncomingRequest>,
) -> Result<get_content_as_filename::v3::Response> {
    let mxc = format!("mxc://{}/{}", body.server_name, body.media_id);

    if let Some(FileMeta {
        content_disposition: _,
        content_type,
        file,
    }) = db.media.get(&db.globals, &mxc).await?
    {
        Ok(get_content_as_filename::v3::Response {
            file,
            content_type,
            content_disposition: Some(format!("inline; filename={}", body.filename)),
        })
    } else if &*body.server_name != db.globals.server_name() && body.allow_remote {
        let remote_content_response =
            get_remote_content(&db, &mxc, &body.server_name, &body.media_id).await?;

        Ok(get_content_as_filename::v3::Response {
            content_disposition: Some(format!("inline: filename={}", body.filename)),
            content_type: remote_content_response.content_type,
            file: remote_content_response.file,
        })
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}

/// # `GET /_matrix/media/r0/thumbnail/{serverName}/{mediaId}`
///
/// Load media thumbnail from our server or over federation.
///
/// - Only allows federation if `allow_remote` is true
pub async fn get_content_thumbnail_route(
    db: DatabaseGuard,
    body: Ruma<get_content_thumbnail::v3::IncomingRequest>,
) -> Result<get_content_thumbnail::v3::Response> {
    let mxc = format!("mxc://{}/{}", body.server_name, body.media_id);

    if let Some(FileMeta {
        content_type, file, ..
    }) = db
        .media
        .get_thumbnail(
            &mxc,
            &db.globals,
            body.width
                .try_into()
                .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
            body.height
                .try_into()
                .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
        )
        .await?
    {
        Ok(get_content_thumbnail::v3::Response { file, content_type })
    } else if &*body.server_name != db.globals.server_name() && body.allow_remote {
        let get_thumbnail_response = db
            .sending
            .send_federation_request(
                &db.globals,
                &body.server_name,
                get_content_thumbnail::v3::Request {
                    allow_remote: false,
                    height: body.height,
                    width: body.width,
                    method: body.method.clone(),
                    server_name: &body.server_name,
                    media_id: &body.media_id,
                },
            )
            .await?;

        db.media
            .upload_thumbnail(
                mxc,
                &db.globals,
                &None,
                &get_thumbnail_response.content_type,
                body.width.try_into().expect("all UInts are valid u32s"),
                body.height.try_into().expect("all UInts are valid u32s"),
                &get_thumbnail_response.file,
            )
            .await?;

        Ok(get_thumbnail_response)
    } else {
        Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."))
    }
}
