use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WorkspaceConfig {
    pub id: String,
    pub name: String,
    pub path: String,
    #[serde(default, rename = "artifactPaths")]
    pub artifact_paths: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PolicyConfig {
    pub max_file_bytes: usize,
    pub max_context_chars: usize,
    pub max_search_results: usize,
    pub max_task_output_chars: usize,
    pub shell_enabled: bool,
    pub allowed_commands: Vec<String>,
    #[serde(default)]
    pub task_retention_hours: Option<i64>,
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
    30_000
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskProfile {
    pub command: Vec<String>,
    pub cwd: Option<String>,
    pub timeout_ms: u64,
    #[serde(default)]
    pub background: bool,
    #[serde(default)]
    pub output_filter: OutputFilter,
}

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceSettings {
    #[serde(default)]
    pub default_path: Option<String>,
    #[serde(default)]
    pub allowed_roots: Vec<String>,
    #[serde(default)]
    pub artifact_paths: Vec<String>,
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
pub struct DaemonConfig {
    #[serde(default)]
    pub workspaces: Vec<WorkspaceConfig>,
    #[serde(default)]
    pub workspace: WorkspaceSettings,
    #[serde(default)]
    pub skills: SkillsConfig,
    pub policy: PolicyConfig,
    pub tasks: HashMap<String, TaskProfile>,
    pub cache_root: String,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize)]
pub struct RpcRequest {
    pub id: u64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
pub struct RpcResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorBody>,
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

#[allow(dead_code)]
impl RpcResponse {
    pub fn success(id: u64, result: Value) -> Self {
        Self {
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }
    pub fn failure(id: u64, error: AppError) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error.0),
        }
    }
}

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
    fn task_profile_new_fields_default_for_existing_configs() {
        let profile: TaskProfile = serde_json::from_value(serde_json::json!({
            "command": ["cargo", "check"],
            "cwd": null,
            "timeoutMs": 120000
        }))
        .unwrap();

        assert!(!profile.background);
        assert!(matches!(profile.output_filter, OutputFilter::Raw));
    }

    #[test]
    fn task_profile_parses_cargo_json_filter() {
        let profile: TaskProfile = serde_json::from_value(serde_json::json!({
            "command": ["cargo", "check", "--message-format=json"],
            "cwd": null,
            "timeoutMs": 120000,
            "background": true,
            "outputFilter": {
                "type": "cargoJson",
                "includeWarnings": true
            }
        }))
        .unwrap();

        assert!(profile.background);
        assert!(matches!(
            profile.output_filter,
            OutputFilter::CargoJson {
                include_warnings: true
            }
        ));
    }
}
