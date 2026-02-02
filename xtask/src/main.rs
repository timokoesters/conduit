mod generate_docs;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
struct Xtask {
    #[clap(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    GenerateDocs,
}

fn main() -> Result<()> {
    match Xtask::parse().cmd {
        Command::GenerateDocs => generate_docs::run(),
    }
}
