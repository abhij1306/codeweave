use anyhow::Result;
use codeweave_rust::{manager::Application, security};
use std::{io, net::TcpListener, path::Path, process::Command as ProcessCommand, sync::Arc};

use crate::token_file;

use super::args::{Cli, Transport};
use super::config::{config_relative_path, load_config, AuthMode, Config};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

fn executable_on_path(command: &str) -> bool {
    let direct = Path::new(command);
    if direct.components().count() > 1 {
        return direct.is_file();
    }
    let extensions: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT".into())
            .split(';')
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .any(|dir| {
            extensions.iter().any(|ext| {
                dir.join(format!("{command}{ext}")).is_file() || dir.join(command).is_file()
            })
        })
}

impl Check {
    fn ok(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: true,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            ok: false,
            detail: detail.into(),
        }
    }
}

/// Run the same preflight work as normal startup, retaining individual failures
/// so `doctor` can explain every actionable problem in one invocation.
pub async fn doctor_checks(cli: &Cli) -> Vec<Check> {
    let mut checks = Vec::new();
    let Config { server, daemon } = match load_config(&cli.config) {
        Ok(value) => {
            checks.push(Check::ok(
                "config",
                format!("parsed {}", cli.config.display()),
            ));
            value
        }
        Err(error) => {
            checks.push(Check::fail(
                "config",
                format!("{error}; fix the JSON or pass --config <path>"),
            ));
            return checks;
        }
    };
    checks.push(Check::ok(
        "auth",
        format!("{} authentication", server.auth_mode.as_str()),
    ));
    checks.push(Check::ok("tools", "fixed 25-tool contract"));

    let workspace_ok = match security::canonical_root(Path::new(&daemon.workspace.path)) {
        Ok(root) => {
            checks.push(Check::ok("workspace", root.display().to_string()));
            true
        }
        Err(error) => {
            checks.push(Check::fail(
                "workspace",
                format!("{error}; set workspace.path to an existing directory"),
            ));
            false
        }
    };

    let git_version =
        tokio::task::spawn_blocking(|| ProcessCommand::new("git").arg("--version").output())
            .await
            .unwrap_or_else(|error| Err(io::Error::other(error)));
    match git_version {
        Ok(output) if output.status.success() => checks.push(Check::ok(
            "git",
            String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        )),
        Ok(output) => checks.push(Check::fail(
            "git",
            format!(
                "git --version exited {}; install Git and add it to PATH",
                output.status
            ),
        )),
        Err(error) => checks.push(Check::fail(
            "git",
            format!("{error}; install Git and add it to PATH"),
        )),
    }

    let intelligence = &daemon.intelligence;
    for (language, check_name, settings, default_command) in [
        (
            "rust",
            "intelligence rust",
            &intelligence.rust,
            "rust-analyzer",
        ),
        (
            "python",
            "intelligence python",
            &intelligence.python,
            "basedpyright-langserver",
        ),
        (
            "typescript",
            "intelligence typescript",
            &intelligence.typescript,
            "typescript-language-server",
        ),
    ] {
        if !settings.enabled {
            checks.push(Check::ok(
                check_name,
                "disabled; syntactic and lexical fallback remain available",
            ));
            continue;
        }
        let command = if settings.command.is_empty() {
            default_command
        } else {
            settings.command.as_str()
        };
        if executable_on_path(command) {
            checks.push(Check::ok(
                check_name,
                format!("{command} is available; server starts lazily"),
            ));
        } else {
            checks.push(Check::fail(check_name, format!("{command} is unavailable; install it, fix intelligence.{language}.command, or disable the adapter")));
        }
    }

    if matches!(cli.transport, Transport::Stdio) {
        checks.push(Check::ok("port", "skipped for stdio transport"));
    } else {
        let host = cli.host.as_deref().unwrap_or(&server.host);
        let port = cli.port.unwrap_or(server.port);
        match TcpListener::bind((host, port)) {
            Ok(listener) => {
                drop(listener);
                checks.push(Check::ok("port", format!("{host}:{port} is available")));
            }
            Err(error) if error.kind() == io::ErrorKind::AddrInUse => checks.push(Check::fail("port", format!("{host}:{port} is already in use; stop the other instance or change server.port"))),
            Err(error) => checks.push(Check::fail("port", format!("cannot bind {host}:{port}: {error}; verify server.host and server.port"))),
        }
    }

    if matches!(cli.transport, Transport::Http) && server.auth_mode == AuthMode::Bearer {
        let token_path = config_relative_path(&cli.config, &server.token_file);
        match token_file::read_private(&token_path) {
            Ok(Some(value)) if !value.trim().is_empty() => checks.push(Check::ok(
                "token",
                format!("{} is present and protected", token_path.display()),
            )),
            Ok(Some(_)) => checks.push(Check::fail(
                "token",
                format!(
                    "{} is empty; delete it and run serve, or write a token",
                    token_path.display()
                ),
            )),
            Ok(None) => checks.push(Check::fail(
                "token",
                format!(
                    "{} is missing; run serve once or run init",
                    token_path.display()
                ),
            )),
            Err(error) => checks.push(Check::fail(
                "token",
                format!("cannot read {}: {error}", token_path.display()),
            )),
        }
    } else {
        checks.push(Check::ok(
            "token",
            "not required for this transport/auth mode",
        ));
    }

    if workspace_ok {
        let manager = Arc::new(Application::default());
        let bash_configured = !daemon.policy.bash.executable.trim().is_empty();
        match manager.initialize(daemon).await {
            Ok(init) => {
                let indexed = init.index_ready;
                let files = init.file_count;
                if indexed {
                    checks.push(Check::ok("index", format!("ready; {files} files indexed")));
                } else {
                    checks.push(Check::fail(
                        "index",
                        "initialization returned index_ready=false",
                    ));
                }
                let bash_available = init.bash_available;
                if bash_configured && bash_available {
                    checks.push(Check::ok(
                        "bash",
                        "available (pre-probed during initialization)",
                    ));
                } else if !bash_configured {
                    checks.push(Check::fail("bash", "policy.bash.executable is empty"));
                } else {
                    checks.push(Check::fail("bash", "configured bash is unavailable; install it or update policy.bash.executable"));
                }
            }
            Err(error) => {
                checks.push(Check::fail(
                    "index",
                    format!("{error}; fix the workspace or index configuration"),
                ));
                checks.push(Check::fail(
                    "bash",
                    "not checked because initialization failed",
                ));
            }
        }
    } else {
        checks.push(Check::fail(
            "index",
            "not checked until workspace.path is fixed",
        ));
        checks.push(Check::fail(
            "bash",
            "not checked until workspace.path is fixed",
        ));
    }
    checks
}

pub async fn run_doctor(cli: &Cli) -> Result<()> {
    let checks = doctor_checks(cli).await;
    let failed = checks.iter().any(|check| !check.ok);
    for check in checks {
        let status = if check.ok { "ok" } else { "FAIL" };
        println!("[{status}] {} — {}", check.name, check.detail);
    }
    if failed {
        anyhow::bail!("doctor found configuration problems")
    }
    Ok(())
}
