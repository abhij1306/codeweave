use anyhow::{Context, Result};
use codeweave_rust::model::{self, DaemonConfig};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use uuid::Uuid;

use crate::token_file;

/// How HTTP clients authenticate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AuthMode {
    /// A bearer token read from `tokenFile`.
    Bearer,
    /// No authentication; allowed only on a loopback host.
    None,
}

impl AuthMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
            Self::None => "none",
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_host")]
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_auth")]
    pub auth_mode: AuthMode,
    #[serde(default = "default_token")]
    pub token_file: String,
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    #[serde(default)]
    pub allowed_origins: Vec<String>,
    /// Idle HTTP keep-alive timeout in milliseconds. Hyper's equivalent of
    /// Uvicorn's `timeout_keep_alive`: an idle kept-alive connection is closed
    /// after this long, so a tunnel/connector does not hold the socket open to
    /// its own ~90s deadline and report that as the connection lifetime. `0`
    /// disables the bound (connections stay open until the peer closes them).
    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
}
pub fn default_host() -> String {
    "127.0.0.1".into()
}
pub fn default_port() -> u16 {
    8813
}
fn default_auth() -> AuthMode {
    AuthMode::Bearer
}
pub fn default_token() -> String {
    ".mcp-token".into()
}
pub fn default_idle_timeout_ms() -> u64 {
    5000
}

/// The `server` section and the daemon configuration, read from one
/// config document.
pub struct Config {
    pub server: ServerConfig,
    pub daemon: DaemonConfig,
}

pub fn load_config(path: &Path) -> Result<Config> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut root: Value = serde_json::from_str(&text).context("parsing config JSON")?;
    let server: ServerConfig =
        serde_json::from_value(root.get("server").cloned().unwrap_or_else(|| json!({})))
            .context("invalid server configuration")?;
    let object = root
        .as_object_mut()
        .context("config root must be an object")?;
    object.entry("cacheRoot").or_insert_with(|| {
        let base = path.parent().unwrap_or_else(|| Path::new("."));
        Value::String(base.join(".codeweave-cache").to_string_lossy().into_owned())
    });
    object.remove("server");
    object.remove("rust");
    let daemon =
        model::parse_daemon_config(&root).map_err(|error| anyhow::anyhow!(error.0.message))?;
    if daemon.config_version != 2 {
        anyhow::bail!(
            "unsupported configVersion {}; configVersion must be 2",
            daemon.config_version
        );
    }
    Ok(Config { server, daemon })
}

pub fn config_relative_path(config_path: &Path, configured_path: &str) -> PathBuf {
    let configured = PathBuf::from(configured_path);
    if configured.is_absolute() {
        configured
    } else {
        config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(configured)
    }
}

pub fn load_or_create_bearer_token(path: &Path) -> Result<String> {
    match token_file::read_private(path)? {
        Some(value) => {
            let token = value.trim();
            if token.is_empty() {
                anyhow::bail!("bearer token file is empty: {}", path.display());
            }
            tracing::info!(path = %path.display(), "bearer authentication loaded");
            Ok(token.to_owned())
        }
        None => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).with_context(|| {
                    format!("creating bearer token directory {}", parent.display())
                })?;
            }
            let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
            token_file::create_private(path, token.as_bytes())?;
            tracing::info!(path = %path.display(), "generated bearer token");
            Ok(token)
        }
    }
}

#[cfg(all(test, windows))]
pub fn validate_token_permissions(path: &Path) -> Result<()> {
    token_file::validate_private(path)
}
