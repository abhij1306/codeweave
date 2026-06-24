use crate::model::{AppError, AppResult};
use std::path::{Component, Path, PathBuf};

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

pub fn resolve_for_write(root: &Path, relative: &str) -> AppResult<PathBuf> {
    let relative = validate_relative(relative)?;
    let joined = root.join(relative);
    let parent = joined
        .parent()
        .ok_or_else(|| AppError::invalid("Target has no parent directory"))?;
    let existing_parent = nearest_existing(parent)?;
    let canonical_parent = existing_parent.canonicalize()?;
    if !canonical_parent.starts_with(root) {
        return Err(AppError::new(
            "OUTSIDE_ROOT",
            "Target parent is outside workspace",
        ));
    }
    Ok(joined)
}

fn nearest_existing(path: &Path) -> AppResult<PathBuf> {
    let mut current = path.to_path_buf();
    loop {
        if current.exists() {
            return Ok(current);
        }
        if !current.pop() {
            return Err(AppError::new(
                "PATH_NOT_FOUND",
                "No existing parent directory",
            ));
        }
    }
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
}
