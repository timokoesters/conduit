pub mod error;

use std::{
    io::{self, IsTerminal},
    path::PathBuf,
};

use clap::{Parser, Subcommand};
use comfy_table::{Table, presets::UTF8_FULL};
use conduit_common::admin::{self, CodeBlock, CommandOutput, MediaFile, RowItem};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

use crate::error::{Error, Result};

/// Returns the current version of the crate with extra info if supplied
///
/// Set the environment variable `CONDUIT_VERSION_EXTRA` to any UTF-8 string to
/// include it in parenthesis after the SemVer version. A common value are git
/// commit hashes.
pub fn version() -> String {
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
    pub command: Option<Command>,
}

#[derive(Serialize, Deserialize)]
pub struct CommandWithStdin {
    pub command: Command,
    pub stdin: Option<String>,
}

#[derive(Subcommand, Serialize, Deserialize)]
pub enum Command {
    #[cfg(unix)]
    #[command(flatten)]
    Admin(admin::Command),
}

impl Command {
    pub fn takes_text_input(&self) -> bool {
        match self {
            Self::Admin(commands) => commands.takes_text_input(),
        }
    }
}

/// Parse command line arguments into structured data
pub fn parse() -> Cli {
    Cli::parse()
}

pub async fn handle_command(socket_path: &PathBuf, command: Command) -> Result<()> {
    let stdin = if command.takes_text_input() {
        if io::stdin().is_terminal() {
            return Err(Error::NoRequiredStdin);
        }

        let mut stdin = String::new();
        tokio::io::stdin().read_to_string(&mut stdin).await?;
        Some(stdin)
    } else {
        None
    };

    let mut stream = UnixStream::connect(socket_path).await?;

    stream.writable().await?;
    let buf = &serde_json::to_vec(&CommandWithStdin { command, stdin })?;
    stream.write_all(buf).await?;

    stream.shutdown().await?;

    let mut buf = Vec::new();
    stream.readable().await?;
    stream.read_to_end(&mut buf).await?;

    let out: Result<CommandOutput, String> = serde_json::from_slice(&buf)?;
    out.unwrap().print().await?;

    Ok(())
}

trait Output {
    async fn print(self) -> Result<()>;
}

impl Output for CommandOutput {
    async fn print(self) -> Result<()> {
        match self {
            Self::Plain(message) => println!("{message}"),
            Self::TaskComplete(message) => println!("{message}"),
            Self::File(MediaFile { file, .. }) => {
                terminal_check()?;

                let mut stdout = tokio::io::stdout();
                stdout.write_all(&file).await?;
                stdout.flush().await?;
            }
            Self::Table {
                title,
                headers,
                page,
                rows,
            } => {
                if rows.is_empty() {
                    println!("{title} has no page {page}")
                } else {
                    let mut table = Table::new();

                    table
                        .load_style(UTF8_FULL.with_rounded_corners())
                        .set_content_arrangement(comfy_table::ContentArrangement::Dynamic)
                        .set_header(headers)
                        .add_rows(rows.into_iter().map(|row| {
                            row.get()
                                .into_iter()
                                .map(stdout_rowitem)
                                .collect::<Vec<_>>()
                        }));

                    println!("{table}")
                }
            }
            Self::CodeBlockWithMsg {
                code_block: CodeBlock { code, language: _ },
                message,
            } => println!("{message}\n{code}"),
        }

        Ok(())
    }
}

fn stdout_rowitem(item: RowItem) -> String {
    match item {
        RowItem::Plain(item) | RowItem::Code(item) => item,
        RowItem::Empty => String::new(),
    }
}

fn terminal_check() -> Result<()> {
    if io::stdout().is_terminal() {
        Err(Error::PrintingBinaryToTerminal)
    } else {
        Ok(())
    }
}
