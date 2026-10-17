use crate::model::{AppError, AppResult};
use crate::security::normalize_path_text;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::json;
use std::path::Path;

/// Configured gitignore-style exclusions plus the built-in ignored directories.
#[derive(Clone, Debug)]
pub struct WorkspaceExclusions {
    normalized_root: String,
    matcher: Gitignore,
    patterns: Vec<String>,
}

impl WorkspaceExclusions {
    pub fn new(root: &Path, patterns: &[String]) -> AppResult<Self> {
        let mut builder = GitignoreBuilder::new(".");
        for pattern in patterns {
            let normalized = pattern.replace('\\', "/");
            if normalized.trim().is_empty() || normalized.starts_with('!') {
                return Err(AppError::details(
                    "INVALID_EXCLUDE_PATTERN",
                    "Workspace exclude patterns must be non-empty exclusions",
                    json!({"pattern": pattern}),
                ));
            }
            builder.add_line(None, &normalized).map_err(|error| {
                AppError::details(
                    "INVALID_EXCLUDE_PATTERN",
                    error.to_string(),
                    json!({"pattern": pattern}),
                )
            })?;
        }
        let matcher = builder.build().map_err(|error| {
            AppError::details(
                "INVALID_EXCLUDE_PATTERN",
                error.to_string(),
                json!({"patterns": patterns}),
            )
        })?;
        Ok(Self {
            normalized_root: normalized_absolute_path(root),
            matcher,
            patterns: patterns.to_vec(),
        })
    }

    pub fn is_ignored(&self, path: &Path, is_dir: bool) -> bool {
        let Some(relative) = self.relative_path(path) else {
            return false;
        };
        ignored_workspace_path(&relative)
            || self
                .matcher
                .matched_path_or_any_parents(Path::new(&relative), is_dir)
                .is_ignore()
    }

    pub(super) fn patterns(&self) -> &[String] {
        &self.patterns
    }

    fn relative_path(&self, path: &Path) -> Option<String> {
        if !path.is_absolute() {
            return Some(normalize_path_text(&path.to_string_lossy()).into_owned());
        }
        let normalized = normalized_absolute_path(path);
        if let Some(relative) = self.relative_from_normalized(&normalized) {
            return Some(relative);
        }
        let canonical = path.canonicalize().ok()?;
        let normalized = normalized_absolute_path(&canonical);
        self.relative_from_normalized(&normalized)
    }

    fn relative_from_normalized(&self, normalized: &str) -> Option<String> {
        let root_len = self.normalized_root.len();
        if normalized.len() < root_len
            || !normalized.as_bytes()[..root_len]
                .eq_ignore_ascii_case(self.normalized_root.as_bytes())
        {
            return None;
        }
        let suffix = normalized.get(root_len..)?;
        if !suffix.is_empty() && !suffix.starts_with('/') {
            return None;
        }
        Some(suffix.trim_start_matches('/').to_owned())
    }
}

fn normalized_absolute_path(path: &Path) -> String {
    let lossy = path.to_string_lossy();
    let normalized = normalize_path_text(&lossy);
    normalized
        .strip_prefix("//?/")
        .unwrap_or(&*normalized)
        .trim_end_matches('/')
        .to_owned()
}

/// Whether a workspace-relative path lies under a built-in ignored directory.
pub fn ignored_workspace_path(path: &str) -> bool {
    normalize_path_text(path)
        .split('/')
        .any(ignored_path_component)
}

pub(super) fn excluded_dir(path: &Path) -> bool {
    path.file_name()
        .and_then(|value| value.to_str())
        .map(ignored_path_component)
        .unwrap_or(false)
}

pub(super) fn excluded_path(path: &Path) -> bool {
    if path
        .file_name()
        .and_then(|value| value.to_str())
        .map(|name| name.contains(".codeweave-"))
        .unwrap_or(false)
    {
        return true;
    }
    path.components().any(|part| {
        part.as_os_str()
            .to_str()
            .map(ignored_path_component)
            .unwrap_or(false)
    })
}

fn ignored_path_component(name: &str) -> bool {
    matches!(
        name,
        ".git"
            | "node_modules"
            | "target"
            | "dist"
            | "build"
            | ".next"
            | ".venv"
            | "venv"
            | ".codeweave-cache"
    ) || name.starts_with("target-")
        || name.starts_with("target_")
}
