use super::edit::PlannedFile;
use cap_std::{ambient_authority, fs::Dir};
use similar::TextDiff;
use std::io;
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

fn workspace_dir(root: &Path) -> io::Result<Dir> {
    Dir::open_ambient_dir(root, ambient_authority())
}

fn relative_path(relative: &str) -> io::Result<std::path::PathBuf> {
    crate::security::validate_relative(relative).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid workspace-relative path: {error}"),
        )
    })
}

pub(super) fn read_optional(root: &Path, relative: &str) -> io::Result<Option<String>> {
    let dir = workspace_dir(root)?;
    let relative = relative_path(relative)?;
    match dir.read_to_string(relative) {
        Ok(content) => Ok(Some(content)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

pub(super) fn remove_if_exists(root: &Path, relative: &str) -> io::Result<()> {
    let dir = workspace_dir(root)?;
    let relative = relative_path(relative)?;
    match dir.remove_file(relative) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

pub(super) fn atomic_write(root: &Path, relative: &str, content: &str) -> io::Result<()> {
    let dir = workspace_dir(root)?;
    let relative = relative_path(relative)?;
    if let Some(parent) = relative.parent() {
        if !parent.as_os_str().is_empty() {
            dir.create_dir_all(parent)?;
        }
    }
    let name = relative
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let temp =
        relative.with_file_name(format!(".{name}.codeweave-{}.tmp", Uuid::new_v4().simple()));
    if let Err(error) = dir.write(&temp, content) {
        let _ = dir.remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = dir.rename(&temp, &dir, &relative) {
        let _ = dir.remove_file(&temp);
        return Err(error);
    }
    Ok(())
}

pub(super) fn restore_one(root: &Path, item: &PlannedFile) -> io::Result<()> {
    match &item.before {
        Some(content) => atomic_write(root, &item.path, content),
        None => remove_if_exists(root, &item.path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn atomic_write_creates_and_replaces_files() {
        let root = tempdir().unwrap();
        atomic_write(root.path(), "nested/value.txt", "first").unwrap();
        atomic_write(root.path(), "nested/value.txt", "second").unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("nested/value.txt")).unwrap(),
            "second"
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let root = tempdir().unwrap();
        let outside = tempdir().unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();

        assert!(atomic_write(root.path(), "escape/value.txt", "blocked").is_err());
        assert!(!outside.path().join("value.txt").exists());
    }
}
