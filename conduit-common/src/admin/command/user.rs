use std::time::Duration;

use clap::{Args, Subcommand};
use ruma::OwnedUserId;
use serde::{Deserialize, Serialize};

/// Commands for managing users on this server
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum Command {
    /// Lists all the users of this server
    List,

    /// Deactivates the specified users
    ///
    /// Users will not be removed from all rooms by default.
    /// Use --leave-rooms to force the users to leave all rooms.
    Deactivate {
        /// User ids of users to deactivate
        user_ids: Vec<OwnedUserId>,

        /// Whether the users should also leave all the rooms they are currently in after being
        /// deactivated.
        #[arg(short, long, default_value = "true")]
        leave_rooms: bool,

        #[command(flatten)]
        purge_media: DeactivatePurgeMediaArgs,
    },

    /// Resets the user's password
    ResetPassword {
        /// User ID of the user for whom the password should be reset
        user_id: OwnedUserId,

        /// The new password to be set for the user, if unspecified one is generated
        password: Option<String>,
    },

    /// Creates a new user
    Create {
        /// User ID of the user
        user_id: OwnedUserId,

        /// Password of the new user, if unspecified one is generated
        password: Option<String>,
    },
}

#[derive(Args, Debug, Serialize, Deserialize)]
#[group(multiple = true, required = false)]
pub struct DeactivatePurgeMediaArgs {
    /// Purges all media uploaded by the user(s) after deactivating their account
    #[arg(long, short = 'm')]
    pub purge_media: bool,

    /// If the --purge-media is present, it only purges media uploaded in the last {time-period}
    ///
    /// Should be in the form specified by humantime::parse_duration
    /// (e.g. 48h, 60min, 10days etc.)
    // --help is unformatted
    #[allow(rustdoc::bare_urls)]
    /// https://docs.rs/humantime/2.2.0/humantime/fn.parse_duration.html
    ///
    /// Note: This will also delete identical media uploaded by other users, so
    /// only use this when all the media they uploaded in this timeframe is undesirable
    #[arg(
        long, short = 't',
        value_parser = humantime::parse_duration,
        requires = "purge_media"
    )]
    pub media_from_last: Option<Duration>,

    /// If the --purge-media is present, it will also delete identical media uploaded by other
    /// users, ensuring that the file is removed from the media backend, so only use this when all
    /// the media they uploaded is undesirable
    #[arg(long, short = 'f', requires = "purge_media")]
    pub force_filehash: bool,
}
