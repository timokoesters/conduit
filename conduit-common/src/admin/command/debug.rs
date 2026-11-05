use clap::Subcommand;
use ruma::{OwnedEventId, RoomVersionId};
use serde::{Deserialize, Serialize};

/// Commands to help with debugging server or general Matrix issues
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum Command {
    #[command(subcommand)]
    Pdu(PduCommand),

    #[command(subcommand)]
    Json(JsonCommand),

    /// List all rooms we are currently handling an incoming pdu from
    IncomingFederation,

    /// Print database memory usage statistics
    MemoryUsage,

    /// Clears all of Conduit's database caches with index smaller than the amount
    ClearDatabaseCaches { amount: u32 },

    /// Clears all of Conduit's service caches with index smaller than the amount
    ClearServiceCaches { amount: u32 },
}

/// Commands related to Matrix Persistent Data Units (PDUs)
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum PduCommand {
    /// Parse and print a PDU from JSON
    ///
    /// The PDU event is only checked for validity and is not added to the
    /// database.
    Parse,

    /// Retrieve and print a PDU by ID from the Conduit database
    Get {
        /// An event ID (a $ followed by the base64 reference hash)
        event_id: OwnedEventId,
    },

    /// Parses a JSON object as an event then creates a hash and signs it, putting a room
    /// version as an argument, and the json in a codeblock if on matrix, or stdin if using the CLI
    HashAndSign { room_version_id: RoomVersionId },

    /// Get the authorization chain of a PDU (the chain of PDUs that authorize this PDU)
    GetAuthChain {
        /// An event ID (the $ character followed by the base64 reference hash)
        event_id: OwnedEventId,
    },
}

/// Commands related to signing and verifying JSON w.r.t. Matrix
#[derive(Subcommand, Debug, Serialize, Deserialize)]
pub enum JsonCommand {
    /// Sign a json object using Conduit's signing keys, putting the json in a codeblock if on
    /// matrix, or stdin if using the CLI
    Sign,

    /// Verify json signatures, putting the json in a codeblock if on matrix, or stdin if using the
    /// CLI
    Verify,
}
