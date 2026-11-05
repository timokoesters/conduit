use std::num::NonZeroUsize;

use clap::Subcommand;
use ruma::{OwnedRoomAliasId, OwnedRoomId, OwnedRoomOrAliasId};
use serde::{Deserialize, Serialize};

/// Commands for managing rooms
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum Command {
    /// List all rooms the server knows about
    List {
        /// The page of results to show
        #[arg(default_value = "1")]
        page: NonZeroUsize,
    },

    #[command(subcommand)]
    Alias(AliasCommand),

    #[command(subcommand)]
    Directory(DirectoryCommand),

    /// Disables incoming federation handling for a room.
    Disable { room_id: OwnedRoomId },
    /// Enables incoming federation handling for a room again.
    Enable { room_id: OwnedRoomId },
}

/// Commands for managing room aliases
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum AliasCommand {
    /// Removes the specified aliases from their rooms, or all the aliases from the specified rooms
    Remove {
        /// The aliases to be removed or the rooms from which to remove all aliases from
        alias: Vec<OwnedRoomOrAliasId>,
    },

    /// Assigns an alias to a room
    Set {
        /// The alias to assign to the room
        alias: OwnedRoomAliasId,

        /// The room id to assign the alias to
        room_id: OwnedRoomId,

        /// Sets the alias even if another room is already using it
        #[arg(short, long)]
        force: bool,
    },

    /// Gets information about a room
    Query { alias: OwnedRoomAliasId },

    /// Lists all rooms known to the server
    List {
        /// The page of results to show
        #[arg(default_value = "1")]
        page: NonZeroUsize,
    },
}

/// Commands for managing the room directory (rooms publicly advertised by the server)
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum DirectoryCommand {
    /// Publishes the specified room to the room directory
    Publish { room: OwnedRoomOrAliasId },

    /// Unpublishes the specified room from the room directory
    Unpublish { room: OwnedRoomOrAliasId },

    /// Lists all the rooms in the room directory
    List {
        /// The page of results to show
        #[arg(default_value = "1")]
        page: NonZeroUsize,
    },
}
