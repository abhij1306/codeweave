use serde::{Deserialize, Serialize};

/// What role a file plays in the workspace, derived from its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentType {
    Source,
    Test,
    Instruction,
    RuntimeEvidence,
    Artifact,
    Log,
}

const TEST_SUFFIXES: &[&str] = &[
    "_test.rs",
    ".test.ts",
    ".spec.ts",
    ".test.tsx",
    ".spec.tsx",
    ".test.js",
    ".spec.js",
    ".test.jsx",
    ".spec.jsx",
];

const ARTIFACT_SUFFIXES: &[&str] = &[
    ".recording.json",
    ".fixture.json",
    ".fixtures.json",
    ".artifact.json",
    ".artifacts.json",
];

impl DocumentType {
    /// Classify a workspace-relative path by whole path segments and file
    /// suffixes, never by arbitrary substrings.
    pub fn classify(path: &str) -> Self {
        let lower = path.to_ascii_lowercase();
        let segments: Vec<&str> = lower.split('/').collect();
        let basename = segments.last().copied().unwrap_or(lower.as_str());
        if basename == "agents.md" || basename == "claude.md" {
            return Self::Instruction;
        }
        if segments
            .iter()
            .any(|segment| matches!(*segment, "test" | "tests" | "__tests__"))
            || basename.starts_with("test_")
            || TEST_SUFFIXES
                .iter()
                .any(|suffix| basename.ends_with(suffix))
        {
            return Self::Test;
        }
        if (segments.contains(&"evidence") || segments.contains(&"runtime"))
            && (basename.ends_with(".json") || basename.ends_with(".log"))
        {
            return Self::RuntimeEvidence;
        }
        if segments.iter().any(|segment| {
            matches!(
                *segment,
                "artifact" | "artifacts" | "fixture" | "fixtures" | "recording" | "recordings"
            )
        }) || ARTIFACT_SUFFIXES
            .iter()
            .any(|suffix| basename.ends_with(suffix))
        {
            return Self::Artifact;
        }
        if basename.ends_with(".log") {
            return Self::Log;
        }
        Self::Source
    }
}
