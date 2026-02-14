use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Error, Debug)]
pub enum Error {
    #[error(
        "The media directory structure depth multiplied by the length is equal to or greater than a sha256 hex hash, please reduce at least one of the two so that their product is less than 64"
    )]
    DirectoryStructureLengthDepthTooLarge,

    #[error("Invalid S3 config")]
    S3,

    #[error("Failed to construct proxy config: {source}")]
    Proxy {
        #[from]
        source: reqwest::Error,
    },

    #[error("Registration token is empty")]
    EmptyRegistrationToken,
}
