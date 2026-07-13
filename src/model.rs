use serde::{Deserialize, Serialize};
use serde_json::Value;
#[derive(Debug, Clone, Deserialize, Serialize)]
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
#[serde(rename_all = "camelCase")]
pub struct PolicyConfig {
    pub max_file_bytes: usize,
    pub max_context_chars: usize,
    pub max_search_results: usize,
    pub bash: BashConfig,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BashConfig {
    #[serde(default)]
    pub enabled: bool,
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
    #[serde(default = "default_bash_retention_hours")]
    pub retention_hours: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum OutputFilter {
    #[default]
    Raw,
    FailedTail {
        #[serde(default = "default_failed_tail_chars")]
        chars: usize,
    },
    TailLines {
        lines: usize,
    },
    CargoJson {
        #[serde(default)]
        include_warnings: bool,
    },
    JsonSummary {
        marker: String,
    },
}

fn default_failed_tail_chars() -> usize {
    default_bash_max_output_chars()
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

fn default_bash_retention_hours() -> i64 {
    1
}

#[cfg(test)]
pub fn test_bash_executable() -> String {
    #[cfg(windows)]
    {
        for root in [
            std::env::var_os("ProgramW6432"),
            std::env::var_os("ProgramFiles"),
        ]
        .into_iter()
        .flatten()
        {
            let candidate = std::path::PathBuf::from(root)
                .join("Git")
                .join("bin")
                .join("bash.exe");
            if candidate.is_file() {
                return candidate.to_string_lossy().into_owned();
            }
        }
    }
    "bash".to_owned()
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct SkillsConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub roots: Vec<String>,
    #[serde(default = "default_explicit_only")]
    pub explicit_only: bool,
}

fn default_explicit_only() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexSettings {
    /// Retrieval ranking algorithm: `"v1"` (legacy additive file scorer) or
    /// `"v2"` (chunk-granular BM25F). Unknown values fall back to `v1`.
    #[serde(default = "default_ranking")]
    pub ranking: String,
}

fn default_ranking() -> String {
    "v1".to_owned()
}

impl Default for IndexSettings {
    fn default() -> Self {
        Self {
            ranking: default_ranking(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DaemonConfig {
    pub workspace: WorkspaceSettings,
    #[serde(default)]
    pub skills: SkillsConfig,
    #[serde(default)]
    pub index: IndexSettings,
    pub policy: PolicyConfig,
    pub cache_root: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

#[derive(Debug)]
pub struct AppError(pub ErrorBody);
pub type AppResult<T> = Result<T, AppError>;

impl AppError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self(ErrorBody {
            code: code.into(),
            message: message.into(),
            details: None,
        })
    }
    pub fn details(code: impl Into<String>, message: impl Into<String>, details: Value) -> Self {
        Self(ErrorBody {
            code: code.into(),
            message: message.into(),
            details: Some(details),
        })
    }
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new("INVALID_ARGUMENT", message)
    }
    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self::new("INTERNAL_ERROR", error.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(value: std::io::Error) -> Self {
        Self::internal(value)
    }
}
impl From<serde_json::Error> for AppError {
    fn from(value: serde_json::Error) -> Self {
        Self::new("INVALID_JSON", value.to_string())
    }
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.0.code, self.0.message)
    }
}

impl std::error::Error for AppError {}

pub fn required_str<'a>(value: &'a Value, key: &str) -> AppResult<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::invalid(format!("Missing or invalid '{key}'")))
}

pub fn bool_value(value: &Value, key: &str, default: bool) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(default)
}

pub fn usize_value(value: &Value, key: &str, default: usize) -> usize {
    value
        .get(key)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(default)
}

pub fn string_list(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
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
            "bash": {
                "enabled": true
            }
        }))
        .unwrap();

        assert!(policy.bash.enabled);
        assert_eq!(policy.bash.executable, "bash");
        assert_eq!(policy.bash.default_timeout_ms, 120_000);
        assert_eq!(policy.bash.foreground_budget_ms, 20_000);
        assert_eq!(policy.bash.max_timeout_ms, 300_000);
        assert_eq!(policy.bash.max_output_chars, 30_000);
        assert_eq!(policy.bash.retention_hours, 1);
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
