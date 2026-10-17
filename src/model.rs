use crate::error::{AppError, AppResult, ErrorCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceConfig {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default, rename = "artifactPaths")]
    pub artifact_paths: Vec<String>,
    #[serde(default, rename = "excludePaths")]
    pub exclude_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PolicyConfig {
    pub max_file_bytes: usize,
    pub max_context_chars: usize,
    pub max_search_results: usize,
    pub bash: BashConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BashConfig {
    #[serde(default = "default_bash_executable")]
    pub executable: String,
    #[serde(default = "default_bash_timeout_ms")]
    pub default_timeout_ms: u64,
    /// Soft cap on how long a foreground `bash` call may block the MCP
    /// request. Commands exceeding it keep running detached and the call
    /// returns a `running` status to poll. Hosted clients (ChatGPT) abort
    /// tool calls at ~60s, so this must stay well below that. 0 disables
    /// auto-promotion.
    #[serde(default = "default_bash_foreground_budget_ms")]
    pub foreground_budget_ms: u64,
    #[serde(default = "default_bash_max_timeout_ms")]
    pub max_timeout_ms: u64,
    #[serde(default = "default_bash_max_output_chars")]
    pub max_output_chars: usize,
}

fn default_bash_executable() -> String {
    "bash".to_owned()
}

fn default_bash_timeout_ms() -> u64 {
    120_000
}

fn default_bash_foreground_budget_ms() -> u64 {
    20_000
}

fn default_bash_max_timeout_ms() -> u64 {
    300_000
}

fn default_bash_max_output_chars() -> usize {
    30_000
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct WorkspaceSettings {
    /// The single repository this server serves for its entire lifetime.
    /// Canonicalized once at startup; there is no runtime switching.
    pub path: String,
    #[serde(default)]
    pub artifact_paths: Vec<String>,
    #[serde(default)]
    pub exclude_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct LanguageServerSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default = "default_lsp_timeout_ms")]
    pub timeout_ms: u64,
}

fn default_lsp_timeout_ms() -> u64 {
    10_000
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IntelligenceSettings {
    #[serde(default)]
    pub rust: LanguageServerSettings,
    #[serde(default)]
    pub python: LanguageServerSettings,
    #[serde(default)]
    pub typescript: LanguageServerSettings,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DaemonConfig {
    pub config_version: u32,
    pub workspace: WorkspaceSettings,
    #[serde(default)]
    pub intelligence: IntelligenceSettings,
    pub policy: PolicyConfig,
    pub cache_root: String,
}

pub fn parse_daemon_config(value: &Value) -> AppResult<DaemonConfig> {
    let encoded = serde_json::to_vec(value).map_err(AppError::internal)?;
    let mut deserializer = serde_json::Deserializer::from_slice(&encoded);
    serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        let path = error.path().to_string();
        AppError::with_details(
            ErrorCode::InvalidConfig,
            format!("invalid configuration at {path}: {}", error.inner()),
            serde_json::json!({"path": path, "error": error.inner().to_string()}),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bash_policy_defaults_optional_fields() {
        let policy: PolicyConfig = serde_json::from_value(serde_json::json!({
            "maxFileBytes": 1000000,
            "maxContextChars": 50000,
            "maxSearchResults": 100,
            "bash": {}
        }))
        .unwrap();

        assert_eq!(policy.bash.executable, "bash");
        assert_eq!(policy.bash.default_timeout_ms, 120_000);
        assert_eq!(policy.bash.foreground_budget_ms, 20_000);
        assert_eq!(policy.bash.max_timeout_ms, 300_000);
        assert_eq!(policy.bash.max_output_chars, 30_000);
    }

    #[test]
    fn workspace_exclusions_parse_and_default_for_existing_configs() {
        let configured: WorkspaceConfig = serde_json::from_value(serde_json::json!({
            "id": "main",
            "name": "Main",
            "path": "/workspace",
            "excludePaths": ["backend/artifacts/", "*.log"]
        }))
        .unwrap();
        assert_eq!(
            configured.exclude_paths,
            vec!["backend/artifacts/", "*.log"]
        );

        let settings: WorkspaceSettings = serde_json::from_value(serde_json::json!({
            "path": "/workspace",
            "excludePaths": ["backend/artifacts/", "*.log"]
        }))
        .unwrap();
        assert_eq!(settings.path, "/workspace");
        assert_eq!(settings.exclude_paths, vec!["backend/artifacts/", "*.log"]);
    }
}
