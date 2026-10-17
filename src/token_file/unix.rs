//! Unix mode bits for the bearer token file.
use super::TokenFileError;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;

pub(super) fn create_private(path: &Path, contents: &[u8]) -> Result<(), TokenFileError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|error| TokenFileError::io("creating", path, error))?;
    file.write_all(contents)
        .map_err(|error| TokenFileError::io("writing", path, error))
}

/// Open `path` without following a final symlink, and only when group and
/// other users have no access.
pub(super) fn open_private(path: &Path) -> Result<Option<File>, TokenFileError> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
            return Err(TokenFileError::Link {
                path: path.to_path_buf(),
            })
        }
        Err(error) => return Err(TokenFileError::io("opening", path, error)),
    };
    let mode = file
        .metadata()
        .map_err(|error| TokenFileError::io("inspecting", path, error))?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(TokenFileError::not_private(
            path,
            format!(
                "must not be accessible by group or other users; run chmod 600 {}",
                path.display()
            ),
        ));
    }
    Ok(Some(file))
}
