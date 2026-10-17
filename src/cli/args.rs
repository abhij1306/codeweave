use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum Transport {
    Http,
    Stdio,
}

#[derive(Parser, Debug)]
#[command(version, about = "Rust-only CodeWeave MCP server")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
    #[arg(long, global = true, default_value = "config.json")]
    pub config: PathBuf,
    #[arg(long, global = true, value_enum, default_value_t = Transport::Http)]
    pub transport: Transport,
    #[arg(long, global = true)]
    pub host: Option<String>,
    #[arg(long, global = true)]
    pub port: Option<u16>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the MCP server.
    Serve,
    /// Create config.json and a bearer token for a project, then print the
    /// connector URL and ChatGPT/Claude next steps.
    Init {
        /// Project directory to serve. Prompted for when omitted.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Overwrite an existing config.json instead of refusing.
        #[arg(long)]
        force: bool,
    },
    /// Run the interactive first-install wizard, validate the repository, and
    /// print a ready-to-paste MCP client configuration.
    Install {
        /// Project directory to serve. Prompted for when omitted.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Replace an existing config without an overwrite prompt.
        #[arg(long)]
        force: bool,
    },
    /// Validate a config end-to-end (config, workspace, git, bash, port, token,
    /// index). Exits non-zero if any check fails.
    Doctor,
}
