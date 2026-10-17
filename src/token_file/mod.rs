//! The bearer token file: created owner-only and refused when anyone else
//! can read it.
//!
//! - Unix: the file is created with mode `0600`. It is opened without
//!   following a symlink and refused when group or other users have any
//!   access.
//! - Windows: the file is created with a protected DACL that grants access
//!   only to the owner, SYSTEM and Administrators. It is opened without
//!   following a reparse point and refused when its owner is not the current
//!   user or its DACL admits anyone else. All `unsafe` Win32 code lives in
//!   `windows.rs`.
//! - Other platforms cannot check privacy, so an existing token file is
//!   refused.
use std::fmt;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as platform;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as platform;

#[cfg(not(any(unix, windows)))]
mod other;
#[cfg(not(any(unix, windows)))]
use other as platform;

#[cfg(test)]
mod tests;

/// Why a token file could not be created or trusted.
#[derive(Debug)]
pub(crate) enum TokenFileError {
    /// A filesystem or OS call failed while `action` the file.
    Io {
        action: &'static str,
        path: PathBuf,
        source: io::Error,
    },
    /// The path is a symlink or reparse point.
    Link { path: PathBuf },
    /// Someone other than the current user can access the file.
    NotPrivate { path: PathBuf, reason: String },
    /// This platform has no supported privacy check.
    #[cfg(not(any(unix, windows)))]
    Unsupported,
}

impl TokenFileError {
    fn io(action: &'static str, path: &Path, source: io::Error) -> Self {
        Self::Io {
            action,
            path: path.to_path_buf(),
            source,
        }
    }

    fn not_private(path: &Path, reason: impl Into<String>) -> Self {
        Self::NotPrivate {
            path: path.to_path_buf(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for TokenFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io {
                action,
                path,
                source,
            } => write!(
                formatter,
                "{action} bearer token file {}: {source}",
                path.display()
            ),
            Self::Link { path } => write!(
                formatter,
                "bearer token file {} must not be a symlink or reparse point",
                path.display()
            ),
            Self::NotPrivate { path, reason } => {
                write!(formatter, "bearer token file {} {reason}", path.display())
            }
            #[cfg(not(any(unix, windows)))]
            Self::Unsupported => formatter.write_str(
                "this platform does not provide a supported private token-file permission check",
            ),
        }
    }
}

impl std::error::Error for TokenFileError {}

/// Create `path` with `contents`, readable only by the current user. Fails
/// when the file already exists.
pub(crate) fn create_private(path: &Path, contents: &[u8]) -> Result<(), TokenFileError> {
    platform::create_private(path, contents)
}

/// The contents of the token file at `path`, or `None` when it does not
/// exist. Fails when the file is a link or not private to the current user.
pub(crate) fn read_private(path: &Path) -> Result<Option<String>, TokenFileError> {
    let Some(mut file) = platform::open_private(path)? else {
        return Ok(None);
    };
    let mut value = String::new();
    file.read_to_string(&mut value)
        .map_err(|error| TokenFileError::io("reading", path, error))?;
    Ok(Some(value))
}
