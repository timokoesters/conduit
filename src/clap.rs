//! Integration with `clap`

use clap::Parser;

/// Command line arguments
#[derive(Parser)]
#[clap(about, version)]
pub struct Args {}

/// Parse command line arguments into structured data
pub fn parse() -> Args {
    Args::parse()
}
