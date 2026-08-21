#[cfg(windows)]
use super::{json, Path, ProcessCommand};

#[cfg(windows)]
fn powershell_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(windows)]
fn run_ngrok_helper(username: &str, password: &str, token: &str) -> std::process::Output {
    use base64::{engine::general_purpose::STANDARD, Engine};

    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.json");
    std::fs::write(
        &config,
        serde_json::to_vec(&json!({
            "server": {
                "authMode": "bearer",
                "port": 8813,
                "tokenFile": ".mcp-token",
                "allowedHosts": ["*"]
            }
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::write(root.path().join(".mcp-token"), token).unwrap();
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("start-ngrok.ps1");
    let command = format!(
        "function global:ngrok {{}}; \
         $credential = [PSCredential]::new({}, (ConvertTo-SecureString {} -AsPlainText -Force)); \
         & {} -Config {} -ClientCredential $credential",
        powershell_literal(username),
        powershell_literal(password),
        powershell_literal(&script.to_string_lossy()),
        powershell_literal(&config.to_string_lossy()),
    );
    let encoded_bytes: Vec<u8> = command.encode_utf16().flat_map(u16::to_le_bytes).collect();

    ProcessCommand::new("pwsh.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-EncodedCommand",
            &STANDARD.encode(encoded_bytes),
        ])
        .output()
        .unwrap()
}

#[test]
fn ngrok_helper_authenticates_public_caller_before_injecting_origin_token() {
    let script = include_str!("../../../start-ngrok.ps1");
    let basic_auth = script.find("type: basic-auth").unwrap();
    let remove_headers = script.find("type: remove-headers").unwrap();
    let inject_origin = script.find("authorization: $OriginAuthorization").unwrap();

    assert!(script.contains("[Parameter(Mandatory = $true)]"));
    assert!(script.contains("enforce: true"));
    assert!(basic_auth < remove_headers);
    assert!(remove_headers < inject_origin);
}

#[cfg(windows)]
#[test]
fn ngrok_helper_rejects_interpolation_in_each_sensitive_input() {
    for (username, password, token, expected) in [
        (
            "user${name}",
            "safe-password",
            "safe-token",
            "public tunnel username",
        ),
        (
            "safe-user",
            "pass${word}",
            "safe-token",
            "public tunnel password",
        ),
        (
            "safe-user",
            "safe-password",
            "token${value}",
            "bearer token",
        ),
    ] {
        let output = run_ngrok_helper(username, password, token);
        let message = format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.status.success(), "{message}");
        assert!(message.contains(expected), "{message}");
        assert!(message.contains("must not contain"), "{message}");
    }
}

#[cfg(windows)]
#[test]
fn ngrok_helper_preserves_valid_credentials_and_token() {
    let output = run_ngrok_helper("safe-user", "safe-password", "safe-token");
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
