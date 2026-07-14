use crate::model::{AppError, AppResult};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PositionEncoding {
    Utf8,
    Utf16,
    Utf32,
}

impl PositionEncoding {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }

    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("utf-8") => Self::Utf8,
            Some("utf-32") => Self::Utf32,
            _ => Self::Utf16,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextDocumentSyncKind {
    None,
    Full,
    Incremental,
}

impl TextDocumentSyncKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Full => "full",
            Self::Incremental => "incremental",
        }
    }

    fn parse(value: Option<&Value>) -> Self {
        let numeric = value.and_then(Value::as_u64).or_else(|| {
            value
                .and_then(|item| item.get("change"))
                .and_then(Value::as_u64)
        });
        match numeric {
            Some(1) => Self::Full,
            Some(2) => Self::Incremental,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServerCapabilities {
    pub(crate) references_provider: bool,
    pub(crate) definition_provider: bool,
    pub(crate) rename_provider: bool,
    pub(crate) diagnostics_provider: bool,
    pub(crate) pull_diagnostics_provider: bool,
    pub(crate) sync_kind: TextDocumentSyncKind,
    pub(crate) position_encoding: PositionEncoding,
    pub(crate) server_name: Option<String>,
    pub(crate) server_version: Option<String>,
    pub(crate) initialization_ms: u128,
}

impl ServerCapabilities {
    pub(crate) fn to_json(&self) -> Value {
        json!({
            "references_provider": self.references_provider,
            "definition_provider": self.definition_provider,
            "rename_provider": self.rename_provider,
            "diagnostics_provider": self.diagnostics_provider,
            "diagnostics_transport": if self.pull_diagnostics_provider { "pull" } else { "publish" },
            "sync_kind": self.sync_kind.as_str(),
            "position_encoding": self.position_encoding.as_str(),
            "server_name": self.server_name,
            "server_version": self.server_version,
            "initialization_ms": self.initialization_ms,
        })
    }

    pub(crate) fn require(&self, operation: &str) -> AppResult<()> {
        let supported = match operation {
            "definition" => self.definition_provider,
            "references" => self.references_provider,
            "rename" => self.rename_provider,
            "diagnostics" => self.diagnostics_provider,
            _ => false,
        };
        if supported {
            Ok(())
        } else {
            Err(AppError::details(
                "LSP_UNSUPPORTED",
                format!("Language server does not advertise {operation} support"),
                json!({"operation": operation}),
            ))
        }
    }
}

fn provider_enabled(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(value)) => *value,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

pub(crate) fn initialize_params(root_uri: &str) -> Value {
    json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "workspaceFolders": [{"uri": root_uri, "name": "workspace"}],
        "capabilities": {
            "general": {
                "positionEncodings": ["utf-8", "utf-16", "utf-32"]
            },
            "workspace": {
                "configuration": true,
                "workspaceFolders": true
            },
            "textDocument": {
                "synchronization": {
                    "dynamicRegistration": false,
                    "didSave": false,
                    "willSave": false,
                    "willSaveWaitUntil": false
                },
                "definition": {"dynamicRegistration": false, "linkSupport": true},
                "references": {"dynamicRegistration": false},
                "rename": {"dynamicRegistration": false, "prepareSupport": false},
                "publishDiagnostics": {
                    "relatedInformation": true,
                    "versionSupport": true,
                    "codeDescriptionSupport": true,
                    "dataSupport": true
                },
                "diagnostic": {"dynamicRegistration": false}
            }
        },
        "clientInfo": {
            "name": "CodeWeave",
            "version": env!("CARGO_PKG_VERSION")
        }
    })
}

pub(crate) fn parse_initialize_result(
    result: &Value,
    initialization_ms: u128,
) -> AppResult<ServerCapabilities> {
    let capabilities = result.get("capabilities").ok_or_else(|| {
        AppError::new(
            "LSP_PROTOCOL_ERROR",
            "Initialize response does not contain capabilities",
        )
    })?;
    let server_info = result.get("serverInfo");
    Ok(ServerCapabilities {
        references_provider: provider_enabled(capabilities.get("referencesProvider")),
        definition_provider: provider_enabled(capabilities.get("definitionProvider")),
        rename_provider: provider_enabled(capabilities.get("renameProvider")),
        diagnostics_provider: provider_enabled(capabilities.get("diagnosticProvider"))
            || capabilities.get("textDocumentSync").is_some(),
        pull_diagnostics_provider: provider_enabled(capabilities.get("diagnosticProvider")),
        sync_kind: TextDocumentSyncKind::parse(capabilities.get("textDocumentSync")),
        position_encoding: PositionEncoding::parse(
            capabilities.get("positionEncoding").and_then(Value::as_str),
        ),
        server_name: server_info
            .and_then(|value| value.get("name"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        server_version: server_info
            .and_then(|value| value.get("version"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        initialization_ms,
    })
}

pub(crate) fn server_request_result(method: &str, params: &Value, root: &Path) -> Value {
    match method {
        "workspace/configuration" => {
            let count = params
                .get("items")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            Value::Array((0..count).map(|_| Value::Null).collect())
        }
        "workspace/workspaceFolders" => json!([{
            "uri": super::normalize::path_uri(root),
            "name": root.file_name().and_then(|name| name.to_str()).unwrap_or("workspace")
        }]),
        "workspace/applyEdit" => json!({
            "applied": false,
            "failureReason": "CodeWeave never applies server-initiated workspace edits"
        }),
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create" => Value::Null,
        _ => Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialize_result_records_capabilities_and_position_encoding() {
        let parsed = parse_initialize_result(
            &json!({
                "capabilities": {
                    "referencesProvider": true,
                    "definitionProvider": {},
                    "renameProvider": false,
                    "diagnosticProvider": {},
                    "textDocumentSync": {"change": 2},
                    "positionEncoding": "utf-8"
                },
                "serverInfo": {"name": "fixture", "version": "1.2.3"}
            }),
            17,
        )
        .unwrap();
        assert!(parsed.references_provider);
        assert!(parsed.definition_provider);
        assert!(!parsed.rename_provider);
        assert!(parsed.diagnostics_provider);
        assert!(parsed.pull_diagnostics_provider);
        assert_eq!(parsed.sync_kind, TextDocumentSyncKind::Incremental);
        assert_eq!(parsed.position_encoding, PositionEncoding::Utf8);
        assert_eq!(parsed.server_name.as_deref(), Some("fixture"));
        assert_eq!(parsed.initialization_ms, 17);
    }

    #[test]
    fn initialize_params_advertise_push_and_pull_diagnostics_support() {
        let params = initialize_params("file:///workspace");
        let text_document = &params["capabilities"]["textDocument"];
        assert_eq!(text_document["publishDiagnostics"]["versionSupport"], true);
        assert_eq!(
            text_document["publishDiagnostics"]["relatedInformation"],
            true
        );
        assert_eq!(text_document["diagnostic"]["dynamicRegistration"], false);
    }
}
