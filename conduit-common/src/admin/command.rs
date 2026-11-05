pub mod debug;
pub mod media;
pub mod room;
pub mod user;

use clap::Subcommand;
use serde::{Deserialize, Serialize};

#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum Command {
    #[command(subcommand)]
    User(user::Command),

    #[command(subcommand)]
    Room(room::Command),

    #[command(subcommand)]
    Media(media::Command),

    #[command(subcommand)]
    Debug(debug::Command),
}

impl Command {
    pub fn takes_text_input(&self) -> bool {
        matches!(
            self,
            Self::Debug(debug::Command::Pdu(debug::PduCommand::Parse))
                | Self::Debug(debug::Command::Json(debug::JsonCommand::Sign))
                | Self::Debug(debug::Command::Json(debug::JsonCommand::Verify))
                | Self::Debug(debug::Command::Pdu(debug::PduCommand::HashAndSign { .. }))
        )
    }
}
