//! The token file through its module interface: what gets created, what is
//! read back, and what is refused.
use super::{create_private, read_private, TokenFileError};

#[test]
fn a_created_token_reads_back() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");

    create_private(&path, b"secret-token").unwrap();

    assert_eq!(
        read_private(&path).unwrap().as_deref(),
        Some("secret-token")
    );
}

#[test]
fn a_missing_token_reads_as_none() {
    let root = tempfile::tempdir().unwrap();

    assert_eq!(read_private(&root.path().join("absent")).unwrap(), None);
}

#[test]
fn creating_over_an_existing_token_fails() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");
    create_private(&path, b"first").unwrap();

    let error = create_private(&path, b"second").unwrap_err();

    assert!(
        matches!(&error, TokenFileError::Io { action: "creating", source, .. }
            if source.kind() == std::io::ErrorKind::AlreadyExists),
        "{error:?}"
    );
    assert_eq!(read_private(&path).unwrap().as_deref(), Some("first"));
}

#[cfg(unix)]
#[test]
fn a_created_token_has_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");

    create_private(&path, b"secret").unwrap();

    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn a_group_readable_token_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");
    create_private(&path, b"secret").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

    let error = read_private(&path).unwrap_err();

    assert!(
        matches!(error, TokenFileError::NotPrivate { .. }),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "bearer token file {0} must not be accessible by group or other users; run chmod 600 {0}",
            path.display()
        )
    );
}

#[cfg(unix)]
#[test]
fn a_symlinked_token_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("target");
    create_private(&target, b"secret").unwrap();
    let link = root.path().join("token");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    let error = read_private(&link).unwrap_err();

    assert!(matches!(error, TokenFileError::Link { .. }), "{error:?}");
}

#[cfg(windows)]
#[test]
fn a_created_token_has_an_owner_only_protected_dacl() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");

    create_private(&path, b"secret").unwrap();

    assert_eq!(
        super::windows::access(&path).unwrap(),
        super::windows::Access {
            owned_by_current_user: true,
            protected: true,
            // OWNER RIGHTS, SYSTEM, Administrators.
            allowed: vec![
                "S-1-3-4".to_owned(),
                "S-1-5-18".to_owned(),
                "S-1-5-32-544".to_owned()
            ],
        }
    );
}

#[cfg(windows)]
#[test]
fn a_token_with_an_inherited_dacl_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");
    std::fs::write(&path, "not-private").unwrap();

    let error = read_private(&path).unwrap_err();

    assert!(
        matches!(error, TokenFileError::NotPrivate { .. }),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!(
            "bearer token file {} must have a protected, explicit Windows DACL",
            path.display()
        )
    );
}
