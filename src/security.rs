//! The workspace boundary.
//!
//! A [`RelPath`] is a workspace-relative path validated and normalized once,
//! where a request or a Git/LSP answer enters. A [`WorkspaceRoot`] is the
//! workspace directory opened once as a cap-std capability; every workspace
//! file read and write goes through it, so a path can never reach outside the
//! root, through `..`, an absolute path, or a symlink or reparse point anywhere
//! along the way.

use crate::error::{AppError, AppResult, ErrorCode};
use cap_std::ambient_authority;
use cap_std::fs::{Dir, File};
use serde::{Serialize, Serializer};
use std::borrow::{Borrow, Cow};
use std::fmt;
use std::io::{self, Read};
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

/// A workspace-relative path: never absolute, never `..`, with `/`
/// separators and no leading `./`. Serializes as the plain string.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(String);

impl RelPath {
    /// Validate and normalize `text`. An absolute path or a `..` component is
    /// `OUTSIDE_ROOT`.
    pub fn new(text: &str) -> AppResult<Self> {
        let normalized = normalize_path_text(text);
        let path = Path::new(normalized.as_ref());
        if path.is_absolute() {
            return Err(AppError::new(
                ErrorCode::OutsideRoot,
                "Absolute paths are not allowed",
            ));
        }
        let escapes = path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        });
        if escapes {
            return Err(AppError::new(
                ErrorCode::OutsideRoot,
                "Path traversal is not allowed",
            ));
        }
        Ok(Self(normalized.into_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl Deref for RelPath {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for RelPath {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl AsRef<Path> for RelPath {
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for RelPath {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

impl PartialEq<str> for RelPath {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

impl PartialEq<&str> for RelPath {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl From<RelPath> for String {
    fn from(path: RelPath) -> Self {
        path.0
    }
}

/// A regular workspace file read through [`WorkspaceRoot::read_file`].
pub struct WorkspaceFile {
    pub bytes: Vec<u8>,
    pub size: u64,
    pub modified_ns: u128,
}

/// The workspace directory, opened once. Clones share the handle.
#[derive(Clone)]
pub struct WorkspaceRoot {
    path: Arc<PathBuf>,
    dir: Arc<Dir>,
}

impl fmt::Debug for WorkspaceRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("WorkspaceRoot")
            .field(&self.path)
            .finish()
    }
}

impl WorkspaceRoot {
    /// Canonicalize `path` and open it as the root capability.
    pub fn open(path: &Path) -> AppResult<Self> {
        let path = canonical_root(path)?;
        let dir = Dir::open_ambient_dir(&path, ambient_authority()).map_err(|error| {
            AppError::with_details(
                ErrorCode::WorkspaceNotFound,
                format!("Cannot open workspace: {error}"),
                serde_json::json!({"path": path}),
            )
        })?;
        Ok(Self {
            path: Arc::new(path),
            dir: Arc::new(dir),
        })
    }

    /// The canonical root on disk, for child processes, the file watcher and
    /// URIs. File access goes through the methods below instead.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// `relative` as an absolute path under the root.
    pub fn join(&self, relative: &RelPath) -> PathBuf {
        self.path.join(relative.as_path())
    }

    /// The canonical relative text of an absolute path under the root.
    pub fn relative(&self, absolute: &Path) -> String {
        relative_string(&self.path, absolute)
    }

    /// Read a regular file of at most `max_bytes`. `None` when it is missing,
    /// not a regular file (a symlink included), or larger than `max_bytes`.
    /// Metadata and bytes come from the same opened handle, which closes
    /// link-swap races between a separate path check and the read.
    pub fn read_file(
        &self,
        relative: &RelPath,
        max_bytes: usize,
    ) -> AppResult<Option<WorkspaceFile>> {
        let Some((file, metadata)) = self.open_regular(relative).map_err(AppError::internal)?
        else {
            return Ok(None);
        };
        let Ok(limit) = usize::try_from(metadata.len()) else {
            return Ok(None);
        };
        if limit > max_bytes {
            return Ok(None);
        }
        let (bytes, truncated) =
            read_bounded(file, max_bytes, limit).map_err(AppError::internal)?;
        if truncated {
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

    /// The first `max_bytes` of a regular file and whether more followed.
    /// `None` when it is missing or not a regular file.
    pub fn read_prefix(
        &self,
        relative: &RelPath,
        max_bytes: usize,
    ) -> io::Result<Option<(Vec<u8>, bool)>> {
        let Some((file, metadata)) = self.open_regular(relative)? else {
            return Ok(None);
        };
        let expected = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        read_bounded(file, max_bytes, expected).map(Some)
    }

    /// The text of a file, or `None` when it does not exist.
    pub fn read_to_string(&self, relative: &RelPath) -> io::Result<Option<String>> {
        match self.dir.read_to_string(relative.as_path()) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The target of a symlink, or `None` when `relative` is not one.
    pub fn read_link(&self, relative: &RelPath) -> io::Result<Option<PathBuf>> {
        match self.dir.symlink_metadata(relative.as_path()) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                self.dir.read_link_contents(relative.as_path()).map(Some)
            }
            Ok(_) => Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Whether `relative` is a regular file (not following a final symlink).
    pub fn is_regular_file(&self, relative: &RelPath) -> bool {
        self.dir
            .symlink_metadata(relative.as_path())
            .is_ok_and(|metadata| metadata.is_file())
    }

    /// When `relative` was last modified, following symlinks inside the root.
    pub fn modified(&self, relative: &RelPath) -> Option<SystemTime> {
        self.dir
            .metadata(relative.as_path())
            .and_then(|metadata| metadata.modified())
            .ok()
            .map(|time| time.into_std())
    }

    /// The absolute path of an existing directory inside the root, for a
    /// child process's working directory. Missing is `PATH_NOT_FOUND`; a
    /// directory reached only through a link out of the root is
    /// `OUTSIDE_ROOT`; anything else that is not a directory is `INVALID_CWD`.
    pub fn directory(&self, relative: &RelPath) -> AppResult<PathBuf> {
        if relative.is_empty() {
            return Ok(self.path.to_path_buf());
        }
        let metadata =
            self.dir
                .metadata(relative.as_path())
                .map_err(|error| match error.kind() {
                    io::ErrorKind::NotFound => AppError::with_details(
                        ErrorCode::PathNotFound,
                        error.to_string(),
                        serde_json::json!({"path": relative}),
                    ),
                    io::ErrorKind::PermissionDenied => AppError::with_details(
                        ErrorCode::OutsideRoot,
                        "Path resolves outside the workspace",
                        serde_json::json!({"path": relative}),
                    ),
                    _ => AppError::internal(error),
                })?;
        if !metadata.is_dir() {
            return Err(AppError::new(
                ErrorCode::InvalidCwd,
                "Bash cwd is not a directory",
            ));
        }
        Ok(self.join(relative))
    }

    /// Replace (or create) a file with `content`: write a sibling temporary
    /// file, then rename it over the target.
    pub fn write_atomic(
        &self,
        relative: &RelPath,
        content: &str,
        temp_name: &str,
    ) -> io::Result<()> {
        let path = relative.as_path();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                self.dir.create_dir_all(parent)?;
            }
        }
        let temp = path.with_file_name(temp_name);
        if let Err(error) = self.dir.write(&temp, content) {
            let _ = self.dir.remove_file(&temp);
            return Err(error);
        }
        if let Err(error) = self.dir.rename(&temp, &self.dir, path) {
            let _ = self.dir.remove_file(&temp);
            return Err(error);
        }
        Ok(())
    }

    /// Remove a file; a missing file is not an error.
    pub fn remove_file(&self, relative: &RelPath) -> io::Result<()> {
        match self.dir.remove_file(relative.as_path()) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn open_regular(
        &self,
        relative: &RelPath,
    ) -> io::Result<Option<(File, cap_std::fs::Metadata)>> {
        match self.dir.symlink_metadata(relative.as_path()) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        }
        let file = self.dir.open(relative.as_path())?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Ok(None);
        }
        Ok(Some((file, metadata)))
    }
}

/// Read at most `max_bytes` of `file`, reserving `expected` bytes up front.
/// Returns the bytes and whether the file held more.
fn read_bounded(file: File, max_bytes: usize, expected: usize) -> io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::with_capacity(expected.min(max_bytes));
    let limit = u64::try_from(max_bytes.saturating_add(1)).unwrap_or(u64::MAX);
    file.take(limit).read_to_end(&mut bytes)?;
    let truncated = bytes.len() > max_bytes;
    bytes.truncate(max_bytes);
    Ok((bytes, truncated))
}

pub fn canonical_root(path: &Path) -> AppResult<PathBuf> {
    let root = path.canonicalize().map_err(|e| {
        AppError::with_details(
            ErrorCode::WorkspaceNotFound,
            format!("Cannot open workspace: {e}"),
            serde_json::json!({"path": path}),
        )
    })?;
    if !root.is_dir() {
        return Err(AppError::new(
            ErrorCode::WorkspaceNotDirectory,
            "Workspace root is not a directory",
        ));
    }
    Ok(root)
}

pub fn relative_string(root: &Path, path: &Path) -> String {
    normalize_path_text(&path.strip_prefix(root).unwrap_or(path).to_string_lossy()).into_owned()
}

/// Canonical textual form of a path: forward slashes and no leading `./`.
/// Index keys, path filters, Git porcelain paths, and exclusion matching all
/// compare paths in this form.
pub fn normalize_path_text(path: &str) -> Cow<'_, str> {
    if path.contains('\\') {
        Cow::Owned(path.replace('\\', "/").trim_start_matches("./").to_owned())
    } else {
        Cow::Borrowed(path.trim_start_matches("./"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_text_uses_forward_slashes_without_leading_dot_segments() {
        assert_eq!(normalize_path_text(".\\src\\main.rs"), "src/main.rs");
        assert_eq!(normalize_path_text("./src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_path_text("C:\\repo\\"), "C:/repo/");
    }

    #[test]
    fn relative_paths_are_normalized_once_and_never_escape() {
        assert_eq!(
            RelPath::new("./src\\main.rs").unwrap().as_str(),
            "src/main.rs"
        );
        assert_eq!(
            serde_json::to_value(RelPath::new("src/lib.rs").unwrap()).unwrap(),
            "src/lib.rs"
        );
        for escaping in ["../secret", "src/../../secret", "..\\secret", "/etc/passwd"] {
            assert_eq!(
                RelPath::new(escaping).unwrap_err().code(),
                ErrorCode::OutsideRoot,
                "{escaping}"
            );
        }
    }

    fn root_with(files: &[(&str, &str)]) -> (tempfile::TempDir, WorkspaceRoot) {
        let temp = tempfile::tempdir().unwrap();
        for (path, content) in files {
            let path = temp.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }
        let root = WorkspaceRoot::open(temp.path()).unwrap();
        (temp, root)
    }

    fn rel(text: &str) -> RelPath {
        RelPath::new(text).unwrap()
    }

    #[test]
    fn confined_reader_reads_regular_workspace_files() {
        let (_temp, root) = root_with(&[("src/main.rs", "fn main() {}\n")]);

        let file = root.read_file(&rel("src/main.rs"), 1024).unwrap().unwrap();

        assert_eq!(file.bytes, b"fn main() {}\n");
        assert_eq!(file.size, 13);
        assert!(root.read_file(&rel("src/main.rs"), 5).unwrap().is_none());
        assert!(root
            .read_file(&rel("src/missing.rs"), 1024)
            .unwrap()
            .is_none());
        assert_eq!(
            root.read_prefix(&rel("src/main.rs"), 5).unwrap(),
            Some((b"fn ma".to_vec(), true))
        );
    }

    #[test]
    fn atomic_write_creates_replaces_and_removes_files() {
        let (temp, root) = root_with(&[]);
        root.write_atomic(&rel("nested/value.txt"), "first", ".tmp-1")
            .unwrap();
        root.write_atomic(&rel("nested/value.txt"), "second", ".tmp-2")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(temp.path().join("nested/value.txt")).unwrap(),
            "second"
        );
        assert_eq!(
            root.read_to_string(&rel("nested/value.txt")).unwrap(),
            Some("second".to_owned())
        );

        root.remove_file(&rel("nested/value.txt")).unwrap();
        root.remove_file(&rel("nested/value.txt")).unwrap();
        assert_eq!(root.read_to_string(&rel("nested/value.txt")).unwrap(), None);
    }

    #[test]
    fn a_working_directory_must_be_an_existing_directory() {
        let (temp, root) = root_with(&[("src/main.rs", "")]);

        assert_eq!(
            root.directory(&rel("src")).unwrap(),
            root.path().join("src")
        );
        assert_eq!(root.directory(&rel("")).unwrap(), root.path());
        assert_eq!(
            root.directory(&rel("missing")).unwrap_err().code(),
            ErrorCode::PathNotFound
        );
        assert_eq!(
            root.directory(&rel("src/main.rs")).unwrap_err().code(),
            ErrorCode::InvalidCwd
        );
        drop(temp);
    }

    #[cfg(unix)]
    mod symlinks {
        use super::*;
        use std::os::unix::fs::symlink;

        #[test]
        fn reads_never_follow_a_link_out_of_the_root() {
            let (temp, root) = root_with(&[]);
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("secret.txt"), "outside-secret").unwrap();
            symlink(
                outside.path().join("secret.txt"),
                temp.path().join("linked.txt"),
            )
            .unwrap();
            symlink(outside.path(), temp.path().join("escape")).unwrap();

            assert!(root.read_file(&rel("linked.txt"), 1024).unwrap().is_none());
            assert!(root
                .read_prefix(&rel("linked.txt"), 1024)
                .unwrap()
                .is_none());
            assert!(root.read_file(&rel("escape/secret.txt"), 1024).is_err());
            assert!(root.read_to_string(&rel("escape/secret.txt")).is_err());
            assert_eq!(
                root.read_link(&rel("linked.txt")).unwrap(),
                Some(outside.path().join("secret.txt"))
            );
            assert_eq!(
                root.directory(&rel("escape")).unwrap_err().code(),
                ErrorCode::OutsideRoot
            );
        }

        #[test]
        fn writes_never_follow_a_link_out_of_the_root() {
            let (temp, root) = root_with(&[]);
            let outside = tempfile::tempdir().unwrap();
            symlink(outside.path(), temp.path().join("escape")).unwrap();

            assert!(root
                .write_atomic(&rel("escape/value.txt"), "blocked", ".tmp")
                .is_err());
            assert!(!outside.path().join("value.txt").exists());
        }

        #[test]
        fn a_directory_swapped_for_a_link_after_open_stays_confined() {
            let (temp, root) = root_with(&[("dir/value.txt", "inside")]);
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("value.txt"), "outside-secret").unwrap();

            std::fs::remove_dir_all(temp.path().join("dir")).unwrap();
            symlink(outside.path(), temp.path().join("dir")).unwrap();

            assert!(root.read_file(&rel("dir/value.txt"), 1024).is_err());
            assert!(root
                .write_atomic(&rel("dir/value.txt"), "blocked", ".tmp")
                .is_err());
            assert_eq!(
                std::fs::read_to_string(outside.path().join("value.txt")).unwrap(),
                "outside-secret"
            );
        }
    }
}
