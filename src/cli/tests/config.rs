//! Config loading and the bearer token file.

use super::super::config::{
    config_relative_path, default_port, load_config, load_or_create_bearer_token, AuthMode, Config,
};
use std::path::{Path, PathBuf};

#[test]
fn relative_token_path_is_resolved_from_config_directory() {
    let config = Path::new("C:/path/to/codeweave/config.json");
    let resolved = config_relative_path(config, ".mcp-token");
    assert_eq!(resolved, PathBuf::from("C:/path/to/codeweave/.mcp-token"));
}

#[cfg(unix)]
#[test]
fn generated_token_is_exclusive_and_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("token");
    let token = load_or_create_bearer_token(&path).unwrap();
    assert!(!token.is_empty());
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(load_or_create_bearer_token(&path).is_err());
}

/// `config.example.json` must deserialize through the *real* config path
/// (`load_config` + `DaemonConfig`) and preserve the runtime contracts the
/// shipped template advertises, including the expected port and safe Host policy.
#[test]
fn shipped_config_example_deserializes_and_matches_runtime_contracts() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("config.example.json");
    let Config { server, daemon } = load_config(&path).expect("config.example.json must load");

    // Port advertised in the example must equal the code default so a copy-paste
    // start hits the port the README and connectors expect.
    assert_eq!(server.port, default_port());
    assert_eq!(server.port, 8813);
    assert_eq!(
        server.allowed_hosts,
        ["localhost", "127.0.0.1", "::1"].map(str::to_owned)
    );
    assert!(!server.allowed_hosts.iter().any(|host| host == "*"));

    // foregroundBudgetMs is present and non-zero (auto-promotion enabled), and
    // the example's bash budget matches the documented code default.
    assert_eq!(daemon.policy.bash.foreground_budget_ms, 20_000);
}

#[test]
fn an_unsupported_auth_mode_fails_config_loading() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.json");
    let mut template: serde_json::Value =
        serde_json::from_str(include_str!("../../../config.example.json")).unwrap();
    template["server"]["authMode"] = "basic".into();
    std::fs::write(&path, serde_json::to_vec(&template).unwrap()).unwrap();
    template["server"]["authMode"] = "none".into();
    let none_path = temp.path().join("none.json");
    std::fs::write(&none_path, serde_json::to_vec(&template).unwrap()).unwrap();

    let error = load_config(&path).err().unwrap();

    assert_eq!(error.to_string(), "invalid server configuration");
    assert!(format!("{error:#}").contains("unknown variant `basic`, expected `bearer` or `none`"));
    assert_eq!(
        load_config(&none_path).unwrap().server.auth_mode,
        AuthMode::None
    );
}
