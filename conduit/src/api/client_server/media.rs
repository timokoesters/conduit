// Unauthenticated media is deprecated
#![allow(deprecated)]

use std::time::Duration;

use crate::{Error, Result, Ruma, services, utils, service::{
    media::{size, FileMeta},
    rate_limiting::Target,
}, };
use http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use ruma::{
    ServerName, UInt,
    api::{
        client::{
            authenticated_media::{
                get_content, get_content_as_filename, get_content_thumbnail, get_media_config,
            },
            error::ErrorKind,
            media::{self, create_content},
        },
        federation::authenticated_media::{self as federation_media, FileOrLocation},
    },
    http_headers::{ContentDisposition, ContentDispositionType},
    media::Method,
};

const MXC_LENGTH: usize = 32;

/// # `GET /_matrix/media/r0/config`
///
/// Returns max upload size.
pub async fn get_media_config_route(
    _body: Ruma<media::get_media_config::v3::Request>,
) -> Result<media::get_media_config::v3::Response> {
    Ok(media::get_media_config::v3::Response {
        upload_size: services().globals.max_request_size().into(),
    })
}

/// # `GET /_matrix/client/v1/media/config`
///
/// Returns max upload size.
pub async fn get_media_config_auth_route(
    _body: Ruma<get_media_config::v1::Request>,
) -> Result<get_media_config::v1::Response> {
    Ok(get_media_config::v1::Response {
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
    let sender_user = body.sender_user.expect("user is authenticated");

    let create_content::v3::Request {
        filename,
        content_type,
        file,
        ..
    } = body.body;

    let target = Target::from_client_request(body.appservice_info, &sender_user);

    services()
        .rate_limiting
        .check_media_upload(target, size(&file)?)
        .await?;

    let media_id = utils::random_string(MXC_LENGTH);

    services()
        .media
        .create(
            services().globals.server_name(),
            &media_id,
            filename.as_deref(),
            content_type.as_deref(),
            &file,
            Some(&sender_user),
        )
        .await?;

    Ok(create_content::v3::Response {
        content_uri: (format!("mxc://{}/{}", services().globals.server_name(), media_id)).into(),
        blurhash: None,
    })
}

pub async fn get_remote_content(
    server_name: &ServerName,
    media_id: String,
    target: Target,
) -> Result<get_content::v1::Response, Error> {
    services()
        .rate_limiting
        .check_media_pre_fetch(&target)
        .await?;

    let content_response = match services()
        .sending
        .send_federation_request(
            server_name,
            federation_media::get_content::v1::Request {
                media_id: media_id.clone(),
                timeout_ms: Duration::from_secs(20),
            },
        )
        .await
    {
        Ok(federation_media::get_content::v1::Response {
            metadata: _,
            content: FileOrLocation::File(content),
        }) => get_content::v1::Response {
            file: content.file,
            content_type: content.content_type,
            content_disposition: content.content_disposition,
        },

        Ok(federation_media::get_content::v1::Response {
            metadata: _,
            content: FileOrLocation::Location(url),
        }) => get_location_content(url).await?,
        Err(Error::BadRequest(ErrorKind::Unrecognized, _)) => {
            let media::get_content::v3::Response {
                file,
                content_type,
                content_disposition,
                ..
            } = services()
                .sending
                .send_federation_request(
                    server_name,
                    media::get_content::v3::Request {
                        server_name: server_name.to_owned(),
                        media_id: media_id.clone(),
                        timeout_ms: Duration::from_secs(20),
                        allow_remote: false,
                        allow_redirect: true,
                    },
                )
                .await?;

            get_content::v1::Response {
                file,
                content_type,
                content_disposition,
            }
        }
        Err(e) => return Err(e),
    };

    services()
        .media
        .create(
            server_name,
            &media_id,
            content_response
                .content_disposition
                .as_ref()
                .and_then(|cd| cd.filename.as_deref()),
            content_response.content_type.as_deref(),
            &content_response.file,
            None,
        )
        .await?;

    services()
        .rate_limiting
        .update_media_post_fetch(target, size(&content_response.file)?)
        .await;

    Ok(content_response)
}

/// # `GET /_matrix/media/r0/download/{serverName}/{mediaId}`
///
/// Load media from our server or over federation.
///
/// - Only allows federation if `allow_remote` is true
pub async fn get_content_route(
    body: Ruma<media::get_content::v3::Request>,
) -> Result<media::get_content::v3::Response> {
    let get_content::v1::Response {
        file,
        content_disposition,
        content_type,
    } = get_content(
        &body.server_name,
        body.media_id.clone(),
        body.sender_ip_address.map(Target::Ip),
    )
    .await?;

    if let Some(target) = Target::from_client_request_optional_auth(
        body.appservice_info,
        &body.sender_user,
        body.sender_ip_address,
    ) {
        services()
            .rate_limiting
            .update_media_post_fetch(target, size(&file)?)
            .await;
    }

    Ok(media::get_content::v3::Response {
        file,
        content_type,
        content_disposition,
        cross_origin_resource_policy: Some("cross-origin".to_owned()),
    })
}

/// # `GET /_matrix/client/v1/media/download/{serverName}/{mediaId}`
///
/// Load media from our server or over federation.
pub async fn get_content_auth_route(
    body: Ruma<get_content::v1::Request>,
) -> Result<get_content::v1::Response> {
    let Ruma::<get_content::v1::Request> {
        body,
        sender_user,
        appservice_info,
        ..
    } = body;

    let sender_user = sender_user.as_ref().expect("user is authenticated");

    let target = Target::from_client_request(appservice_info, sender_user);

    get_content(&body.server_name, body.media_id.clone(), Some(target)).await
}

pub async fn get_content(
    server_name: &ServerName,
    media_id: String,
    target: Option<Target>,
) -> Result<get_content::v1::Response, Error> {
    services().media.check_blocked(server_name, &media_id)?;

    if let Ok(Some(FileMeta {
        content_disposition,
        content_type,
        file,
    })) = services()
        .media
        .get(server_name, &media_id, target.clone())
        .await
    {
        Ok(get_content::v1::Response {
            file,
            content_type,
            content_disposition: Some(content_disposition),
        })
    } else {
        let error = Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."));

        if let Some(target) = target {
            if server_name != services().globals.server_name() && target.is_authenticated() {
                let remote_content_response =
                    get_remote_content(server_name, media_id.clone(), target).await?;

                Ok(get_content::v1::Response {
                    content_disposition: remote_content_response.content_disposition,
                    content_type: remote_content_response.content_type,
                    file: remote_content_response.file,
                })
            } else {
                error
            }
        } else {
            error
        }
    }
}

/// # `GET /_matrix/media/r0/download/{serverName}/{mediaId}/{fileName}`
///
/// Load media from our server or over federation, permitting desired filename.
///
/// - Only allows federation if `allow_remote` is true
pub async fn get_content_as_filename_route(
    body: Ruma<media::get_content_as_filename::v3::Request>,
) -> Result<media::get_content_as_filename::v3::Response> {
    let get_content_as_filename::v1::Response {
        file,
        content_type,
        content_disposition,
    } = get_content_as_filename(
        &body.server_name,
        body.media_id.clone(),
        body.filename.clone(),
        body.sender_ip_address.map(Target::Ip),
    )
    .await?;

    Ok(media::get_content_as_filename::v3::Response {
        file,
        content_type,
        content_disposition,
        cross_origin_resource_policy: Some("cross-origin".to_owned()),
    })
}

/// # `GET /_matrix/client/v1/media/download/{serverName}/{mediaId}/{fileName}`
///
/// Load media from our server or over federation, permitting desired filename.
pub async fn get_content_as_filename_auth_route(
    body: Ruma<get_content_as_filename::v1::Request>,
) -> Result<get_content_as_filename::v1::Response, Error> {
    let Ruma::<get_content_as_filename::v1::Request> {
        body,
        sender_user,
        appservice_info,
        ..
    } = body;

    let sender_user = sender_user.as_ref().expect("user is authenticated");

    let target = Target::from_client_request(appservice_info, sender_user);

    get_content_as_filename(
        &body.server_name,
        body.media_id.clone(),
        body.filename.clone(),
        Some(target),
    )
    .await
}

async fn get_content_as_filename(
    server_name: &ServerName,
    media_id: String,
    filename: String,
    target: Option<Target>,
) -> Result<get_content_as_filename::v1::Response, Error> {
    services().media.check_blocked(server_name, &media_id)?;

    if let Ok(Some(FileMeta {
        file, content_type, ..
    })) = services()
        .media
        .get(server_name, &media_id, target.clone())
        .await
    {
        Ok(get_content_as_filename::v1::Response {
            file,
            content_type,
            content_disposition: Some(
                ContentDisposition::new(ContentDispositionType::Inline)
                    .with_filename(Some(filename.clone())),
            ),
        })
    } else {
        let error = Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."));

        if let Some(target) = target {
            if server_name != services().globals.server_name() && target.is_authenticated() {
                let remote_content_response =
                    get_remote_content(server_name, media_id.clone(), target).await?;

                Ok(get_content_as_filename::v1::Response {
                    content_disposition: Some(
                        ContentDisposition::new(ContentDispositionType::Inline)
                            .with_filename(Some(filename.clone())),
                    ),
                    content_type: remote_content_response.content_type,
                    file: remote_content_response.file,
                })
            } else {
                error
            }
        } else {
            error
        }
    }
}

/// # `GET /_matrix/media/r0/thumbnail/{serverName}/{mediaId}`
///
/// Load media thumbnail from our server or over federation.
///
/// - Only allows federation if `allow_remote` is true
pub async fn get_content_thumbnail_route(
    body: Ruma<media::get_content_thumbnail::v3::Request>,
) -> Result<media::get_content_thumbnail::v3::Response> {
    let Ruma::<media::get_content_thumbnail::v3::Request> {
        body,
        sender_user,
        sender_ip_address,
        appservice_info,
        ..
    } = body;

    let target =
        Target::from_client_request_optional_auth(appservice_info, &sender_user, sender_ip_address);

    let get_content_thumbnail::v1::Response {
        file,
        content_type,
        content_disposition,
    } = get_content_thumbnail(
        &body.server_name,
        body.media_id.clone(),
        body.height,
        body.width,
        body.method.clone(),
        body.animated,
        target,
    )
    .await?;

    Ok(media::get_content_thumbnail::v3::Response {
        file,
        content_type,
        cross_origin_resource_policy: Some("cross-origin".to_owned()),
        content_disposition,
    })
}

/// # `GET /_matrix/client/v1/media/thumbnail/{serverName}/{mediaId}`
///
/// Load media thumbnail from our server or over federation.
pub async fn get_content_thumbnail_auth_route(
    body: Ruma<get_content_thumbnail::v1::Request>,
) -> Result<get_content_thumbnail::v1::Response> {
    let Ruma::<get_content_thumbnail::v1::Request> {
        body,
        sender_user,
        appservice_info,
        ..
    } = body;
    let sender_user = sender_user.as_ref().expect("user is authenticated");
    let target = Target::from_client_request(appservice_info, sender_user);

    get_content_thumbnail(
        &body.server_name,
        body.media_id.clone(),
        body.height,
        body.width,
        body.method.clone(),
        body.animated,
        Some(target),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn get_content_thumbnail(
    server_name: &ServerName,
    media_id: String,
    height: UInt,
    width: UInt,
    method: Option<Method>,
    animated: Option<bool>,
    target: Option<Target>,
) -> Result<get_content_thumbnail::v1::Response, Error> {
    services().media.check_blocked(server_name, &media_id)?;

    if let Some(FileMeta {
        file,
        content_type,
        content_disposition,
    }) = services()
        .media
        .get_thumbnail(
            server_name,
            &media_id,
            width
                .try_into()
                .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Width is invalid."))?,
            height
                .try_into()
                .map_err(|_| Error::BadRequest(ErrorKind::InvalidParam, "Height is invalid."))?,
            target.clone(),
        )
        .await?
    {
        Ok(get_content_thumbnail::v1::Response {
            file,
            content_type,
            content_disposition: Some(content_disposition),
        })
    } else {
        let error = Err(Error::BadRequest(ErrorKind::NotFound, "Media not found."));

        if let Some(target) = target {
            if server_name != services().globals.server_name() {
                services()
                    .rate_limiting
                    .check_media_pre_fetch(&target)
                    .await?;

                let thumbnail_response = match services()
                    .sending
                    .send_federation_request(
                        server_name,
                        federation_media::get_content_thumbnail::v1::Request {
                            height,
                            width,
                            method: method.clone(),
                            media_id: media_id.clone(),
                            timeout_ms: Duration::from_secs(20),
                            animated,
                        },
                    )
                    .await
                {
                    Ok(federation_media::get_content_thumbnail::v1::Response {
                        metadata: _,
                        content: FileOrLocation::File(content),
                    }) => get_content_thumbnail::v1::Response {
                        file: content.file,
                        content_type: content.content_type,
                        content_disposition: content.content_disposition,
                    },

                    Ok(federation_media::get_content_thumbnail::v1::Response {
                        metadata: _,
                        content: FileOrLocation::Location(url),
                    }) => {
                        let get_content::v1::Response {
                            file,
                            content_type,
                            content_disposition,
                        } = get_location_content(url).await?;

                        get_content_thumbnail::v1::Response {
                            file,
                            content_type,
                            content_disposition,
                        }
                    }
                    Err(Error::BadRequest(ErrorKind::Unrecognized, _)) => {
                        let media::get_content_thumbnail::v3::Response {
                            file,
                            content_type,
                            content_disposition,
                            ..
                        } = services()
                            .sending
                            .send_federation_request(
                                server_name,
                                media::get_content_thumbnail::v3::Request {
                                    height,
                                    width,
                                    method: method.clone(),
                                    server_name: server_name.to_owned(),
                                    media_id: media_id.clone(),
                                    timeout_ms: Duration::from_secs(20),
                                    allow_redirect: false,
                                    animated,
                                    allow_remote: false,
                                },
                            )
                            .await?;

                        get_content_thumbnail::v1::Response {
                            file,
                            content_type,
                            content_disposition,
                        }
                    }
                    Err(e) => return Err(e),
                };

                services()
                    .rate_limiting
                    .update_media_post_fetch(target, size(&thumbnail_response.file)?)
                    .await;

                services()
                    .media
                    .upload_thumbnail(
                        server_name,
                        &media_id,
                        thumbnail_response
                            .content_disposition
                            .as_ref()
                            .and_then(|cd| cd.filename.as_deref()),
                        thumbnail_response.content_type.as_deref(),
                        width.try_into().expect("all UInts are valid u32s"),
                        height.try_into().expect("all UInts are valid u32s"),
                        &thumbnail_response.file,
                    )
                    .await?;

                Ok(thumbnail_response)
            } else {
                error
            }
        } else {
            error
        }
    }
}

async fn get_location_content(url: String) -> Result<get_content::v1::Response, Error> {
    let client = services().globals.default_client();
    let response = client.get(url).send().await?;
    let headers = response.headers();

    let content_type = headers
        .get(CONTENT_TYPE)
        .and_then(|header| header.to_str().ok())
        .map(ToOwned::to_owned);

    let content_disposition = headers
        .get(CONTENT_DISPOSITION)
        .map(|header| header.as_bytes())
        .map(TryFrom::try_from)
        .and_then(Result::ok);

    let file = response.bytes().await?.to_vec();

    Ok(get_content::v1::Response {
        file,
        content_type,
        content_disposition,
    })
}
