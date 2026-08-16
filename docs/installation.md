# Installation

## Requirements

- Git
- Rust 1.80 or newer, installed with `rustup`
- An MCP client
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

## Install CodeWeave

```bash
cargo install --git https://github.com/abhij1306/codeweave --locked
```

PowerShell:

```powershell
cargo install --git https://github.com/abhij1306/codeweave --locked
```

## First-run setup and pre-flight

Run the interactive installer. It creates the configuration and private bearer,
validates the real startup path, and prints a ready-to-paste MCP client command:

```bash
codeweave install
```

PowerShell:

```powershell
codeweave install
```

For non-interactive automation, use `codeweave init --path <repository>` followed
by `codeweave doctor`. `init` refuses to replace an existing config unless given
`--force`. `doctor` checks JSON/config validation, the workspace, Git, the HTTP
port, bearer-token permissions, eager index initialization, and Bash; a failed
check produces a non-zero exit.

## Configure

CodeWeave serves exactly one repository, fixed for the process lifetime. Edit `config.json` and set `workspace.path` to the absolute path of that repository; it is canonicalized once at startup. There is no runtime repository switching.

Unix-style example:

```json
{
  "workspace": {
    "path": "/home/user/projects/example",
    "artifactPaths": ["artifacts"],
    "excludePaths": ["**/__pycache__/", "**/.pytest_cache/", "**/.mypy_cache/", "**/.ruff_cache/", "*.log"]
  }
}
```

Windows example:

```json
{
  "workspace": {
    "path": "D:\\Development\\example",
    "artifactPaths": ["artifacts"],
    "excludePaths": ["**/__pycache__/", "**/.pytest_cache/", "**/.mypy_cache/", "**/.ruff_cache/", "*.log"]
  }
}
```

A missing or invalid `workspace.path` produces an actionable startup error (`WORKSPACE_NOT_FOUND` / `WORKSPACE_NOT_DIRECTORY`); the server does not start against a nonexistent repository.

`excludePaths` uses workspace-relative gitignore-style patterns and removes matching files from indexing, watcher reconciliation, and change summaries. Negated (`!`) reinclusion patterns are not supported. Add repository-specific generated paths such as `backend/artifacts/`, `.claude/`, `.serena/`, or `.verity/` only when agents do not need to search them.

`artifactPaths` explicitly includes paths that normal Git ignore rules would omit, so a directory should not appear in both lists.

Do not commit `config.json` or `.mcp-token`.

`server.authMode` accepts only `bearer` or `none`. With HTTP transport and bearer authentication enabled, CodeWeave automatically creates `.mcp-token` on the first run if the configured token file is missing. Existing non-empty token files are reused. Stdio transport neither reads nor creates the bearer-token file.

The token is an internal HTTP origin credential for the CodeWeave server. It is not an LLM API key and should not be entered into ChatGPT, Claude, or another AI client.

CodeWeave always advertises the fixed 25-tool surface. See the configuration
reference in the README and `docs/tools.md`.

## Run

HTTP:

```bash
codeweave serve --transport http --config config.json
```

Stdio:

```bash
codeweave serve --transport stdio --config config.json
```

## Update

```bash
cargo install --git https://github.com/abhij1306/codeweave --locked --force
```

Run `doctor` before replacing a running instance. Older Windows versions may
have created a token file with inherited permissions; if `doctor` rejects it,
stop CodeWeave, remove only the configured token file, and start CodeWeave once
to rotate it with a protected DACL.
