# Installation

## Requirements

- Git
- Rust 1.80 or newer, installed with `rustup`
- A compatible MCP client
- Optional: an HTTPS tunnel or reverse proxy for remote HTTP clients

## Install Rust

### Windows

1. Install Git for Windows.
2. Install Rust using `rustup-init.exe` from the official Rust website.
3. Accept the stable toolchain.
4. Install the Microsoft C++ build tools if prompted.
5. Open a new PowerShell session.

```powershell
rustc --version
cargo --version
git --version
```

### macOS

```bash
xcode-select --install
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

### Linux

Install Git and your distribution's build-essential package, then:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

## Build CodeWeave

```bash
git clone <repository-url>
cd codeweave
cp config.example.json config.json
cargo build --release
```

PowerShell:

```powershell
Copy-Item config.example.json config.json
cargo build --release
```

## Configure

Edit `config.json` and replace all example paths with valid local paths. Keep `allowedRoots` narrower than your home directory whenever possible.

Unix-style example:

```json
{
  "workspace": {
    "defaultPath": "/home/user/projects/example",
    "allowedRoots": ["/home/user/projects"],
    "artifactPaths": ["artifacts"],
    "excludePaths": ["**/__pycache__/", "**/.pytest_cache/", "**/.mypy_cache/", "**/.ruff_cache/", "*.log"]
  }
}
```

Windows example:

```json
{
  "workspace": {
    "defaultPath": "D:\\Development\\example",
    "allowedRoots": ["D:\\Development"],
    "artifactPaths": ["artifacts"],
    "excludePaths": ["**/__pycache__/", "**/.pytest_cache/", "**/.mypy_cache/", "**/.ruff_cache/", "*.log"]
  }
}
```

`excludePaths` uses workspace-relative gitignore-style patterns and removes matching files from indexing, watcher reconciliation, and change summaries. Negated (`!`) reinclusion patterns are not supported. Add repository-specific generated paths such as `backend/artifacts/`, `.claude/`, `.serena/`, or `.verity/` only when agents do not need to search them.

`artifactPaths` explicitly includes paths that normal Git ignore rules would omit, so a directory should not appear in both lists. Per-workspace entries under `workspaces` can override these lists; dynamically opened repositories inherit the values shown under `workspace`.

Do not commit `config.json` or `.mcp-token`.

`server.authMode` accepts only `bearer` or `none`. With HTTP transport and bearer authentication enabled, CodeWeave automatically creates `.mcp-token` on the first run if the configured token file is missing. Existing non-empty token files are reused. Stdio transport neither reads nor creates the bearer-token file.

The token is an internal HTTP origin credential for the CodeWeave server. It is not an LLM API key and should not be entered into ChatGPT, Claude, Perplexity, or another AI client.

## Run

HTTP:

```bash
cargo run --release -- --transport http --config config.json
```

Stdio:

```bash
cargo run --release -- --transport stdio --config config.json
```

## Update

```bash
git pull --ff-only
cargo test --release
cargo build --release
```

Review configuration and release notes before replacing a running instance.
