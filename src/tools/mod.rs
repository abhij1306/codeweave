//! The tool registry: the single source of truth for CodeWeave's advertised
//! tools. It drives four things that used to be hand-maintained in three places
//! (D6 triple-pinning):
//!
//! 1. the `tools/list` payload (was `main.rs::tools()`),
//! 2. the transport's callable-name set (was a hardcoded allowlist in
//!    `mcp_transport.rs`),
//! 3. startup and request schema validation.
//!
//! Add or change a tool in exactly one place — `registry()` — and every
//! consumer updates with it.

pub mod schemas;

use serde_json::{json, Value};

/// Hosted-client safety classification. Each tool maps to exactly one level; the
/// level determines the advertised MCP annotation hints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSafety {
    /// No side effects.
    Read,
    /// Mutates local state only.
    WriteClosed,
    /// May discard local state.
    DestructiveClosed,
    /// Writes and reaches the network.
    WriteOpen,
    /// May run arbitrary commands as the CodeWeave OS user.
    DestructiveOpen,
}

impl ToolSafety {
    fn annotations(self) -> Value {
        let (read_only, destructive, idempotent, open_world) = match self {
            ToolSafety::Read => (true, false, true, false),
            ToolSafety::WriteClosed => (false, false, false, false),
            ToolSafety::DestructiveClosed => (false, true, false, false),
            ToolSafety::WriteOpen => (false, false, false, true),
            ToolSafety::DestructiveOpen => (false, true, false, true),
        };
        json!({
            "readOnlyHint": read_only,
            "destructiveHint": destructive,
            "idempotentHint": idempotent,
            "openWorldHint": open_world
        })
    }
}

/// One advertised tool. `input_schema` is a function pointer into
/// `tools::schemas::*` so the schema has a single definition.
pub struct ToolDefinition {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub safety: ToolSafety,
    pub input_schema: fn() -> Value,
}

impl ToolDefinition {
    fn to_payload(&self) -> Value {
        json!({
            "name": self.name,
            "title": self.title,
            "description": self.description,
            "annotations": self.safety.annotations(),
            "execution": {"taskSupport": "forbidden"},
            "inputSchema": (self.input_schema)()
        })
    }
}

use schemas::{bash, edits, git, intelligence, retrieval, workspace};

/// The complete `tools/list` payload in registry order.
#[allow(dead_code)]
pub fn full_list_payload() -> Value {
    Value::Array(registry().iter().map(ToolDefinition::to_payload).collect())
}

/// Build the fixed tool surface.
pub fn fixed_access() -> ToolAccess {
    ToolAccess {
        list_payload: full_list_payload(),
    }
}

/// Reject fields that are not advertised by the selected public tool. Runtime
/// operation/change validators handle discriminator-specific fields inside
/// arrays; this closes the top-level silent-ignore path.
pub fn validate_input_fields(name: &str, input: &Value) -> crate::model::AppResult<()> {
    let object = input
        .as_object()
        .ok_or_else(|| crate::model::AppError::invalid("tool input must be an object"))?;
    let tool = registry()
        .iter()
        .find(|tool| tool.name == name)
        .ok_or_else(|| crate::model::AppError::invalid(format!("unknown tool '{name}'")))?;
    let schema = (tool.input_schema)();
    let allowed = schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|properties| {
            properties
                .keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>()
        })
        .unwrap_or_default();
    let unknown = object
        .keys()
        .filter(|field| !allowed.contains(field.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        Ok(())
    } else {
        Err(crate::model::AppError::details(
            "UNKNOWN_TOOL_FIELD",
            format!("tool '{name}' received unsupported fields"),
            json!({"unknown": unknown, "allowed": allowed}),
        ))
    }
}

/// The complete, ordered tool set. Order is preserved in `tools/list`.
pub fn registry() -> &'static [ToolDefinition] {
    &[
        ToolDefinition {
            name: "workspace",
            title: "Workspace",
            description: "View this server's repository summary or process-local changes, refresh its index, or inspect diagnostics. The repository is fixed for the server's lifetime.",
            safety: ToolSafety::Read,
            input_schema: workspace::workspace,
        },
        ToolDefinition {
            name: "code_retrieve",
            title: "Code Retrieval",
            description: "Use this single tool for all repository discovery and exact reads. Submit one or more explicit operations: find_file, find_symbol, search_text, find_references, symbols_overview, repo_map, or read.",
            safety: ToolSafety::Read,
            input_schema: retrieval::code_retrieve,
        },
        ToolDefinition {
            name: "code_intelligence",
            title: "Code Intelligence",
            description: "Resolve definitions, references, diagnostics, or a rename preview through the optional semantic backend. Results always label semantic, syntactic, or lexical evidence.",
            safety: ToolSafety::Read,
            input_schema: intelligence::code_intelligence,
        },
        ToolDefinition {
            name: "code_write",
            title: "Write One File",
            description: "Create or overwrite exactly one file. Use expected_hash when replacing an existing file.",
            safety: ToolSafety::WriteClosed,
            input_schema: edits::code_write,
        },
        ToolDefinition {
            name: "code_replace",
            title: "Replace Text in One File",
            description: "Replace exact text in exactly one file. The replacement count is checked before writing.",
            safety: ToolSafety::WriteClosed,
            input_schema: edits::code_replace,
        },
        ToolDefinition {
            name: "code_replace_range",
            title: "Replace Fetched Range in One File",
            description: "Replace the complete line range selected by a code_retrieve read handle in exactly one file.",
            safety: ToolSafety::WriteClosed,
            input_schema: edits::code_replace_range,
        },
        ToolDefinition {
            name: "code_insert",
            title: "Insert Text in One File",
            description: "Insert text before, after, or inside one named symbol in exactly one file.",
            safety: ToolSafety::WriteClosed,
            input_schema: edits::code_insert,
        },
        ToolDefinition {
            name: "code_delete",
            title: "Delete One File",
            description: "Delete exactly one file with an optional content-hash precondition.",
            safety: ToolSafety::DestructiveClosed,
            input_schema: edits::code_delete,
        },
        ToolDefinition {
            name: "code_rename",
            title: "Rename One File",
            description: "Rename exactly one file with an optional content-hash precondition.",
            safety: ToolSafety::WriteClosed,
            input_schema: edits::code_rename,
        },
        ToolDefinition {
            name: "code_preview",
            title: "Preview Code Transaction",
            description: "Preview a multi-file edit transaction and return the diff without writing files.",
            safety: ToolSafety::Read,
            input_schema: edits::code_preview,
        },
        ToolDefinition {
            name: "code_transaction",
            title: "Apply Code Transaction",
            description: "Apply a multi-file edit transaction through the same precondition, non-destructive validation reporting, diff, and internal atomic-recovery engine as the narrow write tools.",
            safety: ToolSafety::WriteClosed,
            input_schema: edits::code_transaction,
        },
        ToolDefinition {
            name: "git_status",
            title: "Git Status",
            description: "Show the working tree status: staged, unstaged, untracked, and partially staged files.",
            safety: ToolSafety::Read,
            input_schema: git::git_status,
        },
        ToolDefinition {
            name: "git_diff",
            title: "Git Diff",
            description: "Show the diff for the working tree or, with staged=true, the staged index. Limit to specific paths when given.",
            safety: ToolSafety::Read,
            input_schema: git::git_diff,
        },
        ToolDefinition {
            name: "git_log",
            title: "Git Log",
            description: "Show recent commit history.",
            safety: ToolSafety::Read,
            input_schema: git::git_log,
        },
        ToolDefinition {
            name: "git_show",
            title: "Git Show",
            description: "Show the patch for one commit ref (default HEAD).",
            safety: ToolSafety::Read,
            input_schema: git::git_show,
        },
        ToolDefinition {
            name: "git_blame",
            title: "Git Blame",
            description: "Show line-by-line authorship for one path, optionally bounded by start_line and end_line.",
            safety: ToolSafety::Read,
            input_schema: git::git_blame,
        },
        ToolDefinition {
            name: "git_preflight",
            title: "Git Preflight",
            description: "Return staged_files, partially_staged_files, and the cached staged diff without committing.",
            safety: ToolSafety::Read,
            input_schema: git::git_preflight,
        },
        ToolDefinition {
            name: "git_stage",
            title: "Git Stage",
            description: "Stage the given paths into the index.",
            safety: ToolSafety::WriteClosed,
            input_schema: git::git_stage,
        },
        ToolDefinition {
            name: "git_commit",
            title: "Git Commit",
            description: "Commit the currently staged changes with the given message.",
            safety: ToolSafety::WriteClosed,
            input_schema: git::git_commit,
        },
        ToolDefinition {
            name: "git_restore",
            title: "Git Restore",
            description: "Discard changes for the given paths, restoring them from the index or HEAD. Requires confirm=true because it overwrites local changes.",
            safety: ToolSafety::DestructiveClosed,
            input_schema: git::git_restore,
        },
        ToolDefinition {
            name: "git_push",
            title: "Git Push",
            description: "Push commits to a remote (default origin) and optional branch. This reaches the network and requires confirm=true, matching git_restore, because it is the only network-facing write.",
            safety: ToolSafety::WriteOpen,
            input_schema: git::git_push,
        },
        ToolDefinition {
            name: "bash",
            title: "Run Bash Command",
            description: "Run one Bash process as the CodeWeave OS user. This is trusted-client functionality, not a sandbox. Commands exceeding the foreground budget continue as the same background run; poll bash_status with its run_id instead of reissuing the command. Output is bounded and timeout terminates the process tree.",
            safety: ToolSafety::DestructiveOpen,
            input_schema: bash::bash,
        },
        ToolDefinition {
            name: "bash_status",
            title: "Bash Run Status",
            description: "Return live or completed state and the retained output tail for a Bash run, including how many prefix characters were discarded.",
            safety: ToolSafety::Read,
            input_schema: bash::bash_status,
        },
        ToolDefinition {
            name: "bash_output",
            title: "Bash Run Output",
            description: "Page retained combined, stdout, or stderr output for a Bash run. Continuations cover only the bounded tail; retention metadata reports any discarded prefix.",
            safety: ToolSafety::Read,
            input_schema: bash::bash_output,
        },
        ToolDefinition {
            name: "bash_cancel",
            title: "Cancel Bash Run",
            description: "Cancel a running background Bash run. Partial output is retained.",
            safety: ToolSafety::WriteClosed,
            input_schema: bash::bash_cancel,
        },
    ]
}
/// Immutable fixed tool payload shared through `AppState`.
#[derive(Debug, Clone)]
pub struct ToolAccess {
    list_payload: Value,
}

impl ToolAccess {
    pub fn list_payload(&self) -> &Value {
        &self.list_payload
    }

    pub fn is_known_tool(name: &str) -> bool {
        registry().iter().any(|tool| tool.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_surface_contains_exactly_25_tools() {
        let names = registry().iter().map(|tool| tool.name).collect::<Vec<_>>();
        assert_eq!(
            names,
            vec![
                "workspace",
                "code_retrieve",
                "code_intelligence",
                "code_write",
                "code_replace",
                "code_replace_range",
                "code_insert",
                "code_delete",
                "code_rename",
                "code_preview",
                "code_transaction",
                "git_status",
                "git_diff",
                "git_log",
                "git_show",
                "git_blame",
                "git_preflight",
                "git_stage",
                "git_commit",
                "git_restore",
                "git_push",
                "bash",
                "bash_status",
                "bash_output",
                "bash_cancel",
            ]
        );
    }

    #[test]
    fn list_payload_shape_uses_only_the_edit_discriminator_union() {
        let access = fixed_access();
        let items = access.list_payload().as_array().unwrap();
        assert_eq!(items.len(), 25);
        for item in items {
            let schema = &item["inputSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
            assert_eq!(item["execution"]["taskSupport"], "forbidden");
            let encoded = schema.to_string();
            for forbidden in ["\"allOf\"", "\"not\"", "\"const\""] {
                assert!(
                    !encoded.contains(forbidden),
                    "{} in {}",
                    forbidden,
                    item["name"]
                );
            }
            let allows_change_union = matches!(
                item["name"].as_str(),
                Some("code_preview" | "code_transaction")
            );
            assert_eq!(encoded.contains("\"oneOf\""), allows_change_union);
        }
    }
}
