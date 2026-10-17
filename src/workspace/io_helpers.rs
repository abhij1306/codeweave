use super::edit::PlannedFile;
use super::state::temp_file_name;
use crate::security::{RelPath, WorkspaceRoot};
use similar::{ChangeTag, TextDiff};
use std::io;
use uuid::Uuid;

pub(super) fn render_diff(plan: &[PlannedFile]) -> String {
    let mut output = String::new();
    for item in plan {
        let before = item.before.as_deref().unwrap_or_default();
        let after = item.after.as_deref().unwrap_or_default();
        let diff = TextDiff::from_lines(before, after)
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", item.path), &format!("b/{}", item.path))
            .to_string();
        output.push_str(&diff);
        if !output.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

/// Per-file added/removed line counts for a planned edit. Cheap to compute from the
/// same before/after buffers `render_diff` uses, and small enough to return even when
/// the full unified diff is capped or omitted.
pub(super) fn diff_stat(plan: &[PlannedFile]) -> Vec<(String, usize, usize)> {
    plan.iter()
        .map(|item| {
            let before = item.before.as_deref().unwrap_or_default();
            let after = item.after.as_deref().unwrap_or_default();
            let diff = TextDiff::from_lines(before, after);
            let mut added = 0usize;
            let mut removed = 0usize;
            for change in diff.iter_all_changes() {
                match change.tag() {
                    ChangeTag::Insert => added += 1,
                    ChangeTag::Delete => removed += 1,
                    ChangeTag::Equal => {}
                }
            }
            (item.path.to_string(), added, removed)
        })
        .collect()
}

/// Write `content` to `relative` through the workspace root, atomically.
pub(super) fn atomic_write(
    root: &WorkspaceRoot,
    relative: &RelPath,
    content: &str,
) -> io::Result<()> {
    let name = relative
        .as_path()
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    root.write_atomic(
        relative,
        content,
        &temp_file_name(name, &Uuid::new_v4().simple().to_string()),
    )
}

/// Put a planned file back to its content before the edit.
pub(super) fn restore_one(root: &WorkspaceRoot, item: &PlannedFile) -> io::Result<()> {
    match &item.before {
        Some(content) => atomic_write(root, &item.path, content),
        None => root.remove_file(&item.path),
    }
}
