use thiserror::Error;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Command will output binary data, please redirect output to a file or another command")]
    PrintingBinaryToTerminal,

    #[error("Command requires input on stdin, while none was given")]
    NoRequiredStdin,

    #[error("Error connecting to unix socket: {0}")]
    IoError(#[from] std::io::Error),

    #[error("Error (de)serliazing content to/from socket: {0}")]
    SerdeError(#[from] serde_json::Error),
}
