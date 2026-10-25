//! Integration with `clap`

use crate::service::admin::Command as AdminCommand;
use clap::{Parser, Subcommand};

/// Returns the current version of the crate with extra info if supplied
///
/// Set the environment variable `CONDUIT_VERSION_EXTRA` to any UTF-8 string to
/// include it in parenthesis after the SemVer version. A common value are git
/// commit hashes.
fn version() -> String {
    let cargo_pkg_version = env!("CARGO_PKG_VERSION");

    match option_env!("CONDUIT_VERSION_EXTRA") {
        Some(x) => format!("{cargo_pkg_version} ({x})"),
        None => cargo_pkg_version.to_owned(),
    }
}

/// Command line arguments
#[derive(Parser)]
#[clap(about, version = version())]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Admin(AdminCommand),
}

/// Parse command line arguments into structured data
pub fn parse() -> Cli {
    Cli::parse()
}
