use crate::model::{AppError, AppResult};
use cap_std::{ambient_authority, fs::Dir};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub struct WorkspaceFile {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub modified_ns: u128,
}

pub fn canonical_root(path: &Path) -> AppResult<PathBuf> {
    let root = path.canonicalize().map_err(|e| {
        AppError::details(
            "WORKSPACE_NOT_FOUND",
            format!("Cannot open workspace: {e}"),
            serde_json::json!({"path": path}),
        )
    })?;
    if !root.is_dir() {
        return Err(AppError::new(
            "WORKSPACE_NOT_DIRECTORY",
            "Workspace root is not a directory",
        ));
    }
    Ok(root)
}

pub fn validate_relative(relative: &str) -> AppResult<PathBuf> {
    let path = Path::new(relative);
    if path.is_absolute() {
        return Err(AppError::new(
            "OUTSIDE_ROOT",
            "Absolute paths are not allowed",
        ));
    }
    for component in path.components() {
        if matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        ) {
            return Err(AppError::new(
                "OUTSIDE_ROOT",
                "Path traversal is not allowed",
            ));
        }
    }
    Ok(path.to_path_buf())
}

pub fn resolve_existing(root: &Path, relative: &str) -> AppResult<PathBuf> {
    let relative = validate_relative(relative)?;
    let joined = root.join(&relative);
    let resolved = joined.canonicalize().map_err(|e| {
        AppError::details(
            "PATH_NOT_FOUND",
            e.to_string(),
            serde_json::json!({"path": relative}),
        )
    })?;
    if !resolved.starts_with(root) {
        return Err(AppError::new(
            "OUTSIDE_ROOT",
            "Resolved path is outside workspace",
        ));
    }
    Ok(resolved)
}

/// Read a regular file through a capability rooted at the workspace.
///
/// The final path must not be a symlink, and cap-std prevents intermediate
/// symlinks or reparse points from escaping `root`. Metadata and bytes come
/// from the same opened handle, which also closes link-swap races between a
/// separate path check and the read.
pub fn read_workspace_file(
    root: &Path,
    relative: &str,
    max_bytes: usize,
) -> AppResult<Option<WorkspaceFile>> {
    let relative = validate_relative(relative)?;
    let dir = Dir::open_ambient_dir(root, ambient_authority()).map_err(AppError::internal)?;
    let path_metadata = match dir.symlink_metadata(&relative) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(AppError::internal(error)),
    };
    if !path_metadata.file_type().is_file() {
        return Ok(None);
    }

    let mut file = dir.open(&relative).map_err(AppError::internal)?;
    let metadata = file.metadata().map_err(AppError::internal)?;
    if metadata.len() > max_bytes as u64 {
        return Ok(None);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(max_bytes.saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .map_err(AppError::internal)?;
    if bytes.len() > max_bytes {
        return Ok(None);
    }
    let modified_ns = metadata
        .modified()
        .ok()
        .and_then(|value| {
            value
                .duration_since(cap_std::time::SystemClock::UNIX_EPOCH)
                .ok()
        })
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    Ok(Some(WorkspaceFile {
        size: metadata.len(),
        modified_ns,
        bytes,
    }))
}

pub fn relative_string(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_parent_traversal() {
        assert!(validate_relative("../secret").is_err());
    }
    #[test]
    fn accepts_normal_relative() {
        assert_eq!(
            validate_relative("src/main.rs").unwrap(),
            PathBuf::from("src/main.rs")
        );
    }

    #[test]
    fn confined_reader_reads_regular_workspace_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("src")).unwrap();
        std::fs::write(root.path().join("src/main.rs"), "fn main() {}\n").unwrap();

        let file = read_workspace_file(root.path(), "src/main.rs", 1024)
            .unwrap()
            .unwrap();

        assert_eq!(file.bytes, b"fn main() {}\n");
        assert_eq!(file.size, file.bytes.len() as u64);
    }

    #[cfg(unix)]
    #[test]
    fn confined_reader_rejects_symlinks_outside_workspace() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "outside-secret").unwrap();
        symlink(
            outside.path().join("secret.txt"),
            root.path().join("linked.txt"),
        )
        .unwrap();

        assert!(read_workspace_file(root.path(), "linked.txt", 1024)
            .unwrap()
            .is_none());
    }
}
