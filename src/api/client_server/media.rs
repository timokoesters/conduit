// Unauthenticated media is deprecated
#![allow(deprecated)]

use std::time::Duration;

use crate::{service::media::FileMeta, services, utils, Error, Result, Ruma};
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
    _body: Ruma<get_media_config::v3::Request>,
) -> Result<get_media_config::v3::Response> {
    Ok(get_media_config::v3::Response {
        upload_size: services().globals.max_request_size().into(),
    })
}

/// # `POST /_matrix/media/r0/upload`
///
/// Permanently save media in the server.
///
/// - Some metadata will be saved in the database
/// - Media will be saved in the media/ directory
pub async fn create_content_route(
    body: Ruma<create_content::v3::Request>,
) -> Result<create_content::v3::Response> {
    let mxc = format!(
        "mxc://{}/{}",
        services().globals.server_name(),
        utils::random_string(MXC_LENGTH)
    );

    services()
        .media
        .create(
            mxc.clone(),
            body.filename
                .as_ref()
                .map(|filename| "inline; filename=".to_owned() + filename)
                .as_deref(),
            body.content_type.as_deref(),
            &body.file,
        )
        .await?;

    Ok(create_content::v3::Response {
        content_uri: mxc.into(),
        blurhash: None,
    })
}

pub async fn get_remote_content(
    mxc: &str,
    server_name: &ruma::ServerName,
    media_id: String,
) -> Result<get_content::v3::Response, Error> {
    let content_response = services()
        .sending
        .send_federation_request(
            server_name,
            get_content::v3::Request {
                allow_remote: false,
                server_name: server_name.to_owned(),
                media_id,
                timeout_ms: Duration::from_secs(20),
                allow_redirect: false,
            },
        )
        .await?;

    services()
        .media
        .create(
            mxc.to_owned(),
            content_response.content_disposition.as_deref(),
            content_response.content_type.as_deref(),
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
    body: Ruma<get_content::v3::Request>,
) -> Result<get_content::v3::Response> {
    let mxc = format!("mxc://{}/{}", body.server_name, body.media_id);

    if let Some(FileMeta {
        content_disposition,
        content_type,
        file,
    }) = services().media.get(mxc.clone()).await?
    {
        Ok(get_content::v3::Response {
            file,
            content_type,
            content_disposition,
            cross_origin_resource_policy: Some("cross-origin".to_owned()),
        })
    } else if &*body.server_name != services().globals.server_name() && body.allow_remote {
        let remote_content_response =
            get_remote_content(&mxc, &body.server_name, body.media_id.clone()).await?;

        Ok(get_content::v3::Response {
            content_disposition: remote_content_response.content_disposition,
            content_type: remote_content_response.content_type,
            file: remote_content_response.file,
            cross_origin_resource_policy: Some("cross-origin".to_owned()),
        })
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
    body: Ruma<get_content_as_filename::v3::Request>,
) -> Result<get_content_as_filename::v3::Response> {
    let mxc = format!("mxc://{}/{}", body.server_name, body.media_id);

    if let Some(FileMeta {
        file, content_type, ..
    }) = services().media.get(mxc.clone()).await?
    {
        Ok(get_content_as_filename::v3::Response {
            file,
            content_type,
            content_disposition: Some(format!("inline; filename={}", body.filename)),
            cross_origin_resource_policy: Some("cross-origin".to_owned()),
        })
    } else if &*body.server_name != services().globals.server_name() && body.allow_remote {
        let remote_content_response =
            get_remote_content(&mxc, &body.server_name, body.media_id.clone()).await?;

        Ok(get_content_as_filename::v3::Response {
            content_disposition: Some(format!("inline: filename={}", body.filename)),
            content_type: remote_content_response.content_type,
            file: remote_content_response.file,
            cross_origin_resource_policy: Some("cross-origin".to_owned()),
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
    body: Ruma<get_content_thumbnail::v3::Request>,
) -> Result<get_content_thumbnail::v3::Response> {
    let mxc = format!("mxc://{}/{}", body.server_name, body.media_id);

    if let Some(FileMeta {
        file, content_type, ..
    }) = services()
        .media
        .get_thumbnail(
            mxc.clone(),
            body.width
                .try_into()
                .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
            body.height
                .try_into()
                .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
        )
        .await?
    {
        Ok(get_content_thumbnail::v3::Response {
            file,
            content_type,
            cross_origin_resource_policy: Some("cross-origin".to_owned()),
        })
    } else if body.server_name != services().globals.server_name() && body.allow_remote {
        let get_thumbnail_response = services()
            .sending
            .send_federation_request(
                &body.server_name,
                get_content_thumbnail::v3::Request {
                    allow_remote: false,
                    height: body.height,
                    width: body.width,
                    method: body.method.clone(),
                    server_name: body.server_name.clone(),
                    media_id: body.media_id.clone(),
                    timeout_ms: Duration::from_secs(20),
                    allow_redirect: false,
                    animated: body.animated,
                },
            )
            .await?;

        services()
            .media
            .upload_thumbnail(
                mxc,
                None,
                get_thumbnail_response.content_type.as_deref(),
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
