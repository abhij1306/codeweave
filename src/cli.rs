mod args;
mod config;
mod doctor;
mod install;
mod mcp_transport;
mod server;

#[cfg(test)]
mod tests;

use anyhow::Result;
use clap::Parser;

use args::{Cli, Command};

const SERVER_NAME: &str = "codeweave";

pub(crate) async fn run() -> Result<()> {
    let cli = Cli::parse();
    match &cli.command {
        None | Some(Command::Serve) => {
            server::init_tracing();
            server::run_serve(cli).await
        }
        Some(Command::Init { path, force }) => {
            let (path, force) = (path.clone(), *force);
            install::run_init(&cli, path, force)
        }
        Some(Command::Install { path, force }) => {
            let (path, force) = (path.clone(), *force);
            install::run_install(&cli, path, force).await
        }
        Some(Command::Doctor) => doctor::run_doctor(&cli).await,
    }
}
