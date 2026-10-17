//! Platforms without a supported privacy check: a token file can be created,
//! but an existing one is never trusted.
use super::TokenFileError;
use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::Path;

pub(super) fn create_private(path: &Path, contents: &[u8]) -> Result<(), TokenFileError> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|error| TokenFileError::io("creating", path, error))?;
    file.write_all(contents)
        .map_err(|error| TokenFileError::io("writing", path, error))
}

pub(super) fn open_private(path: &Path) -> Result<Option<File>, TokenFileError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(TokenFileError::Unsupported),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(TokenFileError::io("inspecting", path, error)),
    }
}
