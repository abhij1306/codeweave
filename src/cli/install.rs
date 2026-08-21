use anyhow::{Context, Result};
use codeweave_rust::security;
use serde_json::{json, Value};
use std::{
    io::{self, Write},
    path::PathBuf,
};

use super::args::{Cli, Transport};
use super::config::{
    config_relative_path, default_port, load_config, load_or_create_bearer_token, ServerConfig,
};
use super::doctor::doctor_checks;

fn prompt(
    input: &mut impl io::BufRead,
    output: &mut impl Write,
    label: &str,
    default: &str,
) -> Result<String> {
    write!(output, "{label} [{default}]: ").context("writing installer prompt")?;
    output.flush().context("flushing installer prompt")?;
    let mut answer = String::new();
    input
        .read_line(&mut answer)
        .context("reading installer response")?;
    let answer = answer.trim();
    Ok(if answer.is_empty() {
        default.to_owned()
    } else {
        answer.to_owned()
    })
}

fn prompt_yes_no(
    input: &mut impl io::BufRead,
    output: &mut impl Write,
    label: &str,
    default: bool,
) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    loop {
        write!(output, "{label} [{suffix}]: ").context("writing installer prompt")?;
        output.flush().context("flushing installer prompt")?;
        let mut answer = String::new();
        input
            .read_line(&mut answer)
            .context("reading installer response")?;
        match answer.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => writeln!(output, "Please answer yes or no.")?,
        }
    }
}

fn requested_project(
    requested_path: Option<PathBuf>,
    input: &mut impl io::BufRead,
    output: &mut impl Write,
) -> Result<PathBuf> {
    let project = match requested_path {
        Some(path) => path,
        None => {
            let cwd = std::env::current_dir().context("reading current directory")?;
            PathBuf::from(prompt(
                input,
                output,
                "Project directory",
                &cwd.to_string_lossy(),
            )?)
        }
    };
    security::canonical_root(&project).map_err(|error| anyhow::anyhow!(error))
}

struct InitializedConfig {
    project: PathBuf,
    server: ServerConfig,
    token_path: PathBuf,
}

fn write_initial_config(
    cli: &Cli,
    project: PathBuf,
    force: bool,
    port: Option<u16>,
) -> Result<InitializedConfig> {
    if cli.config.exists() && !force {
        anyhow::bail!(
            "{} already exists; rerun with --force to replace it",
            cli.config.display()
        );
    }

    let mut template: Value = serde_json::from_str(include_str!("../../config.example.json"))
        .context("parsing embedded config.example.json")?;
    template["workspace"]["path"] = Value::String(project.to_string_lossy().into_owned());
    if let Some(port) = port {
        template["server"]["port"] = json!(port);
    }
    let rendered =
        serde_json::to_string_pretty(&template).context("serializing config template")?;
    if let Some(parent) = cli
        .config
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&cli.config, format!("{rendered}\n"))
        .with_context(|| format!("writing {}", cli.config.display()))?;

    let (server, _) = load_config(&cli.config)?;
    let token_path = config_relative_path(&cli.config, &server.token_file);
    if server.auth_mode == "bearer" {
        load_or_create_bearer_token(&token_path)?;
    }
    Ok(InitializedConfig {
        project,
        server,
        token_path,
    })
}

pub(super) fn run_init(cli: &Cli, requested_path: Option<PathBuf>, force: bool) -> Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let project = requested_project(requested_path, &mut input, &mut output)?;
    let initialized = write_initial_config(cli, project, force, None)?;
    writeln!(
        output,
        "Created {} for {}.",
        cli.config.display(),
        initialized.project.display()
    )?;
    writeln!(
        output,
        "Local MCP URL: http://{}:{}/mcp",
        initialized.server.host, initialized.server.port
    )?;
    if initialized.server.auth_mode == "bearer" {
        writeln!(
            output,
            "Origin bearer token: {}",
            initialized.token_path.display()
        )?;
    }
    writeln!(
        output,
        "Next: codeweave serve --config {}",
        cli.config.display()
    )?;
    writeln!(output, "Then follow docs/connect-chatgpt.md or docs/connect-claude.md for an authenticated HTTPS gateway.")?;
    Ok(())
}

pub(super) async fn run_install_with_io(
    cli: &Cli,
    requested_path: Option<PathBuf>,
    force: bool,
    input: &mut impl io::BufRead,
    output: &mut impl Write,
) -> Result<()> {
    writeln!(output, "CodeWeave interactive installation")?;
    writeln!(output, "-------------------------------")?;
    let project = requested_project(requested_path, input, output)?;
    let overwrite = if cli.config.exists() && !force {
        prompt_yes_no(
            input,
            output,
            &format!("Replace existing {}?", cli.config.display()),
            false,
        )?
    } else {
        force
    };
    if cli.config.exists() && !overwrite {
        anyhow::bail!("installation canceled; existing config was left unchanged");
    }

    writeln!(output, "How will your MCP client connect?")?;
    writeln!(
        output,
        "  1) stdio — local clients; no listening port (recommended)"
    )?;
    writeln!(
        output,
        "  2) HTTP  — loopback server for an authenticated gateway"
    )?;
    let transport = loop {
        match prompt(input, output, "Connection", "1")?.as_str() {
            "1" | "stdio" => break Transport::Stdio,
            "2" | "http" => break Transport::Http,
            _ => writeln!(output, "Choose 1 (stdio) or 2 (HTTP).")?,
        }
    };
    let port = if transport == Transport::Http {
        loop {
            let value = prompt(
                input,
                output,
                "Local HTTP port",
                &default_port().to_string(),
            )?;
            match value.parse::<u16>() {
                Ok(port) if port > 0 => break Some(port),
                _ => writeln!(output, "Enter a port from 1 to 65535.")?,
            }
        }
    } else {
        None
    };

    let initialized = write_initial_config(cli, project, true, port)?;
    writeln!(output, "\nValidating the generated installation...")?;
    let validation_cli = Cli {
        command: None,
        config: cli.config.clone(),
        transport,
        host: cli.host.clone(),
        port: port.or(cli.port),
    };
    let checks = doctor_checks(&validation_cli).await;
    let failed = checks.iter().any(|check| !check.ok);
    for check in &checks {
        let status = if check.ok { "ok" } else { "FAIL" };
        writeln!(output, "[{status}] {} — {}", check.name, check.detail)?;
    }
    if failed {
        anyhow::bail!("installation was created, but validation failed; fix the checks above and run `codeweave doctor`");
    }

    let executable = std::env::current_exe().context("resolving the CodeWeave executable")?;
    let config = std::fs::canonicalize(&cli.config).unwrap_or_else(|_| cli.config.clone());
    writeln!(
        output,
        "\nInstallation complete for {}.",
        initialized.project.display()
    )?;
    match transport {
        Transport::Stdio => {
            let client = json!({
                "command": executable,
                "args": ["serve", "--transport", "stdio", "--config", config]
            });
            writeln!(
                output,
                "Paste this command configuration into a local MCP client:"
            )?;
            writeln!(output, "{}", serde_json::to_string_pretty(&client)?)?;
        }
        Transport::Http => {
            writeln!(
                output,
                "Start: codeweave serve --transport http --config {}",
                config.display()
            )?;
            writeln!(
                output,
                "Local URL: http://{}:{}/mcp",
                initialized.server.host, initialized.server.port
            )?;
            writeln!(
                output,
                "Origin token: {} (keep private; a public gateway must authenticate callers separately)",
                initialized.token_path.display()
            )?;
        }
    }
    Ok(())
}

pub(super) async fn run_install(
    cli: &Cli,
    requested_path: Option<PathBuf>,
    force: bool,
) -> Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    run_install_with_io(cli, requested_path, force, &mut input, &mut output).await
}
