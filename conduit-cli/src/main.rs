use std::path::PathBuf;

use clap::Parser;
use conduit_cli::Command;

#[derive(Parser)]
#[clap(about, version = conduit_cli::version())]
struct Cli {
    #[command(subcommand)]
    pub command: Command,

    #[arg(short, long, default_value_os_t = conduit_config::default_socket_path())]
    pub socket_path: PathBuf,
}

#[tokio::main]
async fn main() {
    let args = Cli::parse();

    if let Err(err) = conduit_cli::handle_command(&args.socket_path, args.command).await {
        eprintln!("{err}");
        std::process::exit(1);
    }
}
