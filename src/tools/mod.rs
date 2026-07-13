//! The tool registry: the single source of truth for CodeWeave's advertised
//! tools. It drives four things that used to be hand-maintained in three places
//! (D6 triple-pinning):
//!
//! 1. the `tools/list` payload (was `main.rs::tools()`),
//! 2. the transport's callable-name set (was a hardcoded allowlist in
//!    `mcp_transport.rs`),
//! 3. profile filtering (`server.toolProfile`),
//! 4. the startup schema-shape validation.
//!
//! Add or change a tool in exactly one place — `registry()` — and every
//! consumer updates with it.

pub mod schemas;

use serde_json::{json, Value};
use std::collections::BTreeSet;

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

/// Named tool profiles selectable via `server.toolProfile`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Full,
    ReadOnly,
    Edit,
}

impl Profile {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "full" => Some(Profile::Full),
            "read-only" | "read_only" => Some(Profile::ReadOnly),
            "edit" => Some(Profile::Edit),
            _ => None,
        }
    }
}

/// One advertised tool. `input_schema` is a function pointer into
/// `tools::schemas::*` so the schema has a single definition.
pub struct ToolDefinition {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub safety: ToolSafety,
    /// Named profiles this tool belongs to. `Full` membership is implicit — every
    /// tool is in `full` — so this lists only the restricted profiles.
    pub profiles: &'static [Profile],
    pub input_schema: fn() -> Value,
}

impl ToolDefinition {
    fn in_profile(&self, profile: Profile) -> bool {
        profile == Profile::Full || self.profiles.contains(&profile)
    }

    /// True when this is one of the bash* tools (used to decide whether edit
    /// `validate` commands can run under the active profile).
    pub fn is_bash(&self) -> bool {
        self.name.starts_with("bash")
    }

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

/// The full, unfiltered `tools/list` payload (every advertised tool, in
/// registry order). Used for schema-shape tests and as the `full` profile body.
#[cfg(test)]
pub fn full_list_payload() -> Value {
    Value::Array(registry().iter().map(ToolDefinition::to_payload).collect())
}

/// The complete, ordered tool set. Order is preserved in `tools/list`.
pub fn registry() -> &'static [ToolDefinition] {
    use Profile::{Edit, ReadOnly};
    &[
        ToolDefinition {
            name: "workspace",
            title: "Workspace",
            description: "View this server's repository summary or changes, refresh its index, inspect diagnostics, or explicitly list/read configured skills. The repository is fixed for the server's lifetime (configured in config.json). Skills must only be used when the user explicitly asks.",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: workspace::workspace,
        },
        ToolDefinition {
            name: "code_retrieve",
            title: "Code Retrieval",
            description: "Use this single tool for all repository discovery and exact reads. Submit one or more explicit operations: find_file, find_symbol, search_text, find_references, symbols_overview, repo_map, or read.",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: retrieval::code_retrieve,
        },
        ToolDefinition {
            name: "code_capabilities",
            title: "CodeWeave Capabilities",
            description: "Return the code_retrieve operation contract, edit capabilities, limits, workspace identity, and known limitations.",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: retrieval::code_capabilities,
        },
        ToolDefinition {
            name: "code_intelligence",            title: "Code Intelligence",
            description: "Resolve definitions, references, diagnostics, or a rename preview through the optional semantic backend. Results always label semantic, syntactic, or lexical evidence.",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: intelligence::code_intelligence,
        },
        ToolDefinition {
            name: "code_write",
            title: "Write One File",
            description: "Create or overwrite exactly one file. Use expected_hash when replacing an existing file.",
            safety: ToolSafety::WriteClosed,
            profiles: &[Edit],
            input_schema: edits::code_write,
        },
        ToolDefinition {
            name: "code_replace",
            title: "Replace Text in One File",
            description: "Replace exact text in exactly one file. The replacement count is checked before writing.",
            safety: ToolSafety::WriteClosed,
            profiles: &[Edit],
            input_schema: edits::code_replace,
        },
        ToolDefinition {
            name: "code_replace_range",
            title: "Replace Fetched Range in One File",
            description: "Replace the complete line range selected by a code_retrieve read handle in exactly one file.",
            safety: ToolSafety::WriteClosed,
            profiles: &[Edit],
            input_schema: edits::code_replace_range,
        },
        ToolDefinition {
            name: "code_insert",
            title: "Insert Text in One File",
            description: "Insert text before, after, or inside one named symbol in exactly one file.",
            safety: ToolSafety::WriteClosed,
            profiles: &[Edit],
            input_schema: edits::code_insert,
        },
        ToolDefinition {
            name: "code_delete",
            title: "Delete One File",
            description: "Delete exactly one file with an optional content-hash precondition.",
            safety: ToolSafety::DestructiveClosed,
            profiles: &[Edit],
            input_schema: edits::code_delete,
        },
        ToolDefinition {
            name: "code_rename",
            title: "Rename One File",
            description: "Rename exactly one file with an optional content-hash precondition.",
            safety: ToolSafety::WriteClosed,
            profiles: &[Edit],
            input_schema: edits::code_rename,
        },
        ToolDefinition {
            name: "code_preview",
            title: "Preview Code Transaction",
            description: "Preview a multi-file edit transaction and return the diff without writing files.",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: edits::code_preview,
        },
        ToolDefinition {
            name: "code_transaction",
            title: "Apply Code Transaction",
            description: "Apply a multi-file edit transaction through the same precondition, non-destructive validation reporting, diff, and internal atomic-recovery engine as the narrow write tools.",
            safety: ToolSafety::WriteClosed,
            profiles: &[Edit],
            input_schema: edits::code_transaction,
        },
        ToolDefinition {
            name: "git_status",
            title: "Git Status",
            description: "Show the working tree status: staged, unstaged, untracked, and partially staged files.",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: git::git_status,
        },
        ToolDefinition {
            name: "git_diff",
            title: "Git Diff",
            description: "Show the diff for the working tree or, with staged=true, the staged index. Limit to specific paths when given.",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: git::git_diff,
        },
        ToolDefinition {
            name: "git_log",
            title: "Git Log",
            description: "Show recent commit history.",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: git::git_log,
        },
        ToolDefinition {
            name: "git_show",
            title: "Git Show",
            description: "Show the patch for one commit ref (default HEAD).",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: git::git_show,
        },
        ToolDefinition {
            name: "git_blame",
            title: "Git Blame",
            description: "Show line-by-line authorship for one path, optionally bounded by start_line and end_line.",
            safety: ToolSafety::Read,
            profiles: &[ReadOnly, Edit],
            input_schema: git::git_blame,
        },
        ToolDefinition {
            name: "git_preflight",
            title: "Git Preflight",
            description: "Return staged_files, partially_staged_files, and the cached staged diff without committing.",
            safety: ToolSafety::Read,
            profiles: &[Edit],
            input_schema: git::git_preflight,
        },
        ToolDefinition {
            name: "git_stage",
            title: "Git Stage",
            description: "Stage the given paths into the index.",
            safety: ToolSafety::WriteClosed,
            profiles: &[Edit],
            input_schema: git::git_stage,
        },
        ToolDefinition {
            name: "git_commit",
            title: "Git Commit",
            description: "Commit the currently staged changes with the given message.",
            safety: ToolSafety::WriteClosed,
            profiles: &[Edit],
            input_schema: git::git_commit,
        },
        ToolDefinition {
            name: "git_restore",
            title: "Git Restore",
            description: "Discard changes for the given paths, restoring them from the index or HEAD. Requires confirm=true because it overwrites local changes.",
            safety: ToolSafety::DestructiveClosed,
            profiles: &[Edit],
            input_schema: git::git_restore,
        },
        ToolDefinition {
            name: "git_push",
            title: "Git Push",
            description: "Push commits to a remote (default origin) and optional branch. This reaches the network and requires confirm=true, matching git_restore, because it is the only network-facing write.",
            safety: ToolSafety::WriteOpen,
            // Excluded from `edit`: it is the only network-facing write.
            profiles: &[],
            input_schema: git::git_push,
        },
        ToolDefinition {
            name: "bash",
            title: "Run Bash Command",
            description: "Run a Bash command as the CodeWeave OS user. This is trusted-client functionality, not a sandbox. When the configured foreground budget is enabled (default about 20s), commands that exceed it automatically continue in the background: the call returns status \"running\" with a run_id and detached:true. When that happens, do NOT re-issue the command — poll bash_status(run_id) until the status is terminal. Re-sending an identical command while it is still running returns the same run_id (deduplicated), not a second run. Output is capped at maxOutputChars (default 30000) and each run's default timeout is defaultTimeoutMs (default 120000). Use background:true for known long-running commands.",
            safety: ToolSafety::DestructiveOpen,
            // Excluded from `edit`: edit profile is bash-free.
            profiles: &[],
            input_schema: bash::bash,
        },
        ToolDefinition {
            name: "bash_status",
            title: "Bash Run Status",
            description: "Return live or completed state and the retained output tail for a Bash run.",
            safety: ToolSafety::Read,
            profiles: &[],
            input_schema: bash::bash_status,
        },
        ToolDefinition {
            name: "bash_output",
            title: "Bash Run Output",
            description: "Page retained combined, stdout, or stderr output for a Bash run.",
            safety: ToolSafety::Read,
            profiles: &[],
            input_schema: bash::bash_output,
        },
        ToolDefinition {
            name: "bash_cancel",
            title: "Cancel Bash Run",
            description: "Cancel a running background Bash run. Partial output is retained.",
            safety: ToolSafety::WriteClosed,
            profiles: &[],
            input_schema: bash::bash_cancel,
        },
    ]
}

/// Resolved, immutable view of which tools the running server exposes. Computed
/// once at startup from `server.toolProfile` (+ optional custom include/exclude)
/// and shared read-only through `AppState`.
#[derive(Debug, Clone)]
pub struct ToolAccess {
    allowed: BTreeSet<String>,
    list_payload: Value,
    bash_tools_available: bool,
}

impl ToolAccess {
    /// Whether `name` is callable under the active profile.
    pub fn is_allowed(&self, name: &str) -> bool {
        self.allowed.contains(name)
    }

    /// The pre-rendered `tools/list` payload (array of advertised tools),
    /// already filtered to the active profile.
    pub fn list_payload(&self) -> &Value {
        &self.list_payload
    }

    /// Whether bash tools are callable under the active profile AND enabled by
    /// policy. When false, edit `validate` commands cannot run, so the
    /// compatibility layer rejects edits that carry them.
    pub fn bash_tools_available(&self) -> bool {
        self.bash_tools_available
    }

    /// Whether `name` is a real tool at all (in any profile). Distinguishes
    /// "unknown tool" from "known tool, not in this profile".
    pub fn is_known_tool(name: &str) -> bool {
        registry().iter().any(|tool| tool.name == name)
    }
}

/// A custom include/exclude selection layered over the full set.
#[derive(Debug, Clone, Default)]
pub struct CustomSelection {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

/// Resolve the active tool set. `profile` is `None` for the `custom` profile, in
/// which case `custom` selects over the full set. `policy_bash_enabled` reflects
/// `policy.bash.enabled`.
pub fn resolve_access(
    profile: Option<Profile>,
    custom: &CustomSelection,
    policy_bash_enabled: bool,
) -> Result<ToolAccess, String> {
    let all = registry();
    let allowed: BTreeSet<String> = match profile {
        Some(profile) => all
            .iter()
            .filter(|tool| tool.in_profile(profile))
            .map(|tool| tool.name.to_owned())
            .collect(),
        None => {
            // custom: start from full, apply include (if non-empty as an
            // allowlist) then exclude.
            for name in custom.include.iter().chain(custom.exclude.iter()) {
                if !ToolAccess::is_known_tool(name) {
                    return Err(format!("server.tools references unknown tool '{name}'"));
                }
            }
            let mut set: BTreeSet<String> = if custom.include.is_empty() {
                all.iter().map(|tool| tool.name.to_owned()).collect()
            } else {
                custom.include.iter().cloned().collect()
            };
            for name in &custom.exclude {
                set.remove(name);
            }
            set
        }
    };

    let list_payload = Value::Array(
        all.iter()
            .filter(|tool| allowed.contains(tool.name))
            .map(ToolDefinition::to_payload)
            .collect(),
    );

    let bash_tools_available = policy_bash_enabled
        && all
            .iter()
            .filter(|tool| tool.is_bash())
            .all(|tool| allowed.contains(tool.name));

    Ok(ToolAccess {
        allowed,
        list_payload,
        bash_tools_available,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_in(profile: Option<Profile>) -> Vec<String> {
        let access = resolve_access(profile, &CustomSelection::default(), true).unwrap();
        registry()
            .iter()
            .map(|tool| tool.name.to_owned())
            .filter(|name| access.is_allowed(name))
            .collect()
    }

    #[test]
    fn full_profile_exposes_every_tool_in_registry_order() {
        let full = names_in(Some(Profile::Full));
        let expected: Vec<String> = registry().iter().map(|t| t.name.to_owned()).collect();
        assert_eq!(full, expected);
        assert_eq!(full.len(), 26);
    }

    #[test]
    fn read_only_profile_is_reads_plus_read_git() {
        let read_only = names_in(Some(Profile::ReadOnly));
        assert_eq!(
            read_only,
            vec![
                "workspace",
                "code_retrieve",
                "code_capabilities",
                "code_intelligence",
                "code_preview",
                "git_status",
                "git_diff",
                "git_log",
                "git_show",
                "git_blame",
            ]
        );
    }

    #[test]
    fn edit_profile_excludes_bash_and_git_push() {
        let edit = names_in(Some(Profile::Edit));
        assert!(!edit.iter().any(|name| name.starts_with("bash")));
        assert!(!edit.contains(&"git_push".to_owned()));
        assert!(edit.contains(&"code_write".to_owned()));
        assert!(edit.contains(&"git_commit".to_owned()));
    }

    #[test]
    fn edit_and_read_only_profiles_report_bash_unavailable() {
        let edit = resolve_access(Some(Profile::Edit), &CustomSelection::default(), true).unwrap();
        assert!(!edit.bash_tools_available());
        let read_only =
            resolve_access(Some(Profile::ReadOnly), &CustomSelection::default(), true).unwrap();
        assert!(!read_only.bash_tools_available());
        let full = resolve_access(Some(Profile::Full), &CustomSelection::default(), true).unwrap();
        assert!(full.bash_tools_available());
    }

    #[test]
    fn full_profile_with_bash_policy_disabled_reports_bash_unavailable() {
        let full = resolve_access(Some(Profile::Full), &CustomSelection::default(), false).unwrap();
        assert!(!full.bash_tools_available());
    }

    #[test]
    fn custom_include_is_an_allowlist_and_exclude_subtracts() {
        let selection = CustomSelection {
            include: vec!["code_retrieve".into(), "code_intelligence".into()],
            exclude: vec!["code_intelligence".into()],
        };
        let access = resolve_access(None, &selection, true).unwrap();
        assert!(access.is_allowed("code_retrieve"));
        assert!(!access.is_allowed("code_intelligence"));
        assert!(!access.is_allowed("bash"));
    }

    #[test]
    fn custom_empty_include_defaults_to_full_minus_exclude() {
        let selection = CustomSelection {
            include: vec![],
            exclude: vec!["bash".into(), "git_push".into()],
        };
        let access = resolve_access(None, &selection, true).unwrap();
        assert!(access.is_allowed("code_write"));
        assert!(!access.is_allowed("bash"));
        assert!(!access.is_allowed("git_push"));
    }

    #[test]
    fn custom_rejects_unknown_tool_names() {
        let selection = CustomSelection {
            include: vec!["not_a_tool".into()],
            exclude: vec![],
        };
        assert!(resolve_access(None, &selection, true).is_err());
    }

    #[test]
    fn list_payload_shape_is_valid_and_flat() {
        let access =
            resolve_access(Some(Profile::Full), &CustomSelection::default(), true).unwrap();
        let items = access.list_payload().as_array().unwrap();
        assert_eq!(items.len(), 26);
        for item in items {
            let schema = &item["inputSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
            assert_eq!(item["execution"]["taskSupport"], "forbidden");
            let encoded = schema.to_string();
            for forbidden in ["\"oneOf\"", "\"allOf\"", "\"not\"", "\"const\""] {
                assert!(
                    !encoded.contains(forbidden),
                    "{} in {}",
                    forbidden,
                    item["name"]
                );
            }
        }
    }

    #[test]
    fn profile_parse_accepts_documented_names_only() {
        assert_eq!(Profile::parse("full"), Some(Profile::Full));
        assert_eq!(Profile::parse("read-only"), Some(Profile::ReadOnly));
        assert_eq!(Profile::parse("edit"), Some(Profile::Edit));
        assert_eq!(Profile::parse("nonsense"), None);
    }
}
