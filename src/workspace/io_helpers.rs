use super::edit::PlannedFile;
use similar::TextDiff;
use std::fs;
use std::path::Path;
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

pub(super) fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let temp = path.with_file_name(format!(".{name}.codeweave-{}.tmp", Uuid::new_v4().simple()));
    if let Err(error) = fs::write(&temp, content) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    if path.exists() {
        let backup =
            path.with_file_name(format!(".{name}.codeweave-{}.bak", Uuid::new_v4().simple()));
        if let Err(error) = fs::rename(path, &backup) {
            let _ = fs::remove_file(&temp);
            return Err(error);
        }
        if let Err(error) = fs::rename(&temp, path) {
            let restore = fs::rename(&backup, path);
            let _ = fs::remove_file(&temp);
            if let Err(restore_error) = restore {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("replacement failed: {error}; recovery failed: {restore_error}; original retained at {}", backup.display()),
                ));
            }
            return Err(error);
        }
        if let Err(error) = fs::remove_file(&backup) {
            eprintln!(
                "atomic write backup cleanup failed for {}: {error}",
                backup.display()
            );
        }
    } else if let Err(error) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

pub(super) fn restore_one(root: &Path, item: &PlannedFile) -> std::io::Result<()> {
    let path = root.join(&item.path);
    match &item.before {
        Some(content) => atomic_write(&path, content),
        None if path.exists() => fs::remove_file(path),
        None => Ok(()),
    }
}
