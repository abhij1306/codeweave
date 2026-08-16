# CodeWeave

[![CI](https://github.com/abhij1306/codeweave/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/abhij1306/codeweave/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/Rust-stable-orange.svg)](https://www.rust-lang.org/)
[![Model Context Protocol](https://img.shields.io/badge/Model_Context_Protocol-MCP-7c3aed.svg)](https://modelcontextprotocol.io/)

CodeWeave is a local-first MCP server that gives an AI coding client precise
search, guarded edits, Git operations, semantic intelligence, and bounded Bash
execution inside one repository.

It is built for developers who want capable coding tools without silently
switching repositories, hiding partial edits, or sending an entire machine to
a remote service.

## Quick start

You need [Rust](https://rustup.rs/), Git, and Bash. Windows users can use the
Bash bundled with Git for Windows.

```bash
cargo install --git https://github.com/abhij1306/codeweave --locked
codeweave install
```

The interactive installer:

- asks which repository CodeWeave should serve;
- recommends local stdio or configures loopback HTTP;
- creates `config.json` and a private origin token;
- checks Git, Bash, the workspace, configuration, token permissions, port, and
  eager index startup;
- prints a ready-to-paste MCP client command.

For automation, skip the wizard:

```bash
codeweave init --path /absolute/path/to/repository --config config.json
codeweave doctor --config config.json
```

## Connect a client

### Local clients: stdio

Stdio is the simplest and safest transport. The installer prints a block like:

```json
{
  "command": "/absolute/path/to/codeweave",
  "args": ["serve", "--transport", "stdio", "--config", "/absolute/path/to/config.json"]
}
```

Paste it into any MCP client that accepts a local command.

### Loopback HTTP

```bash
codeweave serve --transport http --config config.json
```

The local endpoint is `http://127.0.0.1:8813/mcp` by default. HTTP requires the
bearer stored in `.mcp-token`; the token is a private origin credential, not an
LLM API key and not external user authentication.

### Remote clients

Never publish the loopback origin by simply injecting its bearer into every
public request. A public gateway must authenticate each external caller first,
then add the private origin bearer for the hop to CodeWeave.

The bundled PowerShell helper uses mandatory HTTP Basic authentication at
ngrok before it substitutes the origin bearer:

```powershell
.\start-ngrok.ps1 -Config .\config.json
```

PowerShell prompts for the public username and password. Use this helper only
with clients that support Basic credentials. URL-only hosted connectors need an
OAuth-capable MCP gateway instead. See [ChatGPT setup](docs/connect-chatgpt.md)
and [Claude setup](docs/connect-claude.md).

## What CodeWeave exposes

CodeWeave advertises one fixed 25-tool surface:

- discovery: `workspace`, `code_retrieve`, `code_intelligence`;
- guarded editing: `code_write`, `code_replace`, `code_replace_range`,
  `code_insert`, `code_delete`, `code_rename`, `code_preview`,
  `code_transaction`;
- Git: `git_status`, `git_diff`, `git_log`, `git_show`, `git_blame`,
  `git_preflight`, `git_stage`, `git_commit`, `git_restore`, `git_push`;
- commands: `bash`, `bash_status`, `bash_output`, `bash_cancel`.

The repository is fixed for the process lifetime. All connected clients share
the same workspace mutations, generation, and Bash runs.

## Safety model

CodeWeave is privileged developer tooling. An authenticated client can run
unsandboxed Bash as your OS account, so connect only clients and models you
trust.

The server still enforces narrower invariants around that trust:

- file reads and writes are capability-confined to the configured workspace;
- snapshot hashes, range handles, and expected-content checks guard edits;
- multi-file edits report partial application and recovery failures honestly;
- Git path arguments and destructive operations use validation and explicit
  confirmation;
- HTTP authentication happens before a bounded 4 MiB request-body read;
- the origin accepts HTTP/1 only and caps concurrent connections;
- bearer files use `0600` on Unix and a protected explicit DACL on Windows.

Read [SECURITY.md](SECURITY.md) before exposing HTTP beyond loopback.

## Configuration

`codeweave install` creates a strict `configVersion: 2` configuration. Unknown
fields are rejected, including unknown nested fields. See
[config.example.json](config.example.json) for every option.

Useful commands:

```bash
codeweave doctor --config config.json
codeweave serve --transport stdio --config config.json
codeweave serve --transport http --config config.json
```

Optional semantic backends are `rust-analyzer`, `basedpyright-langserver`, and
`typescript-language-server`. They are disabled until enabled in the generated
configuration.

Do not commit `config.json`, `.mcp-token`, or `.codeweave-cache/`.

## Updating

Install the newest release from GitHub:

```bash
cargo install --git https://github.com/abhij1306/codeweave --locked --force
codeweave doctor --config /path/to/config.json
```

Older Windows releases created bearer files with inherited permissions. If
`doctor` rejects such a file, stop CodeWeave, remove only the configured token
file, and start it again so the token is rotated with a protected DACL.

## Developing

```bash
git clone https://github.com/abhij1306/codeweave.git
cd codeweave
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets
```

Architecture and concurrency guarantees live in
[docs/architecture.md](docs/architecture.md). Tool contracts are documented in
[docs/tools.md](docs/tools.md), and contributions are welcome through
[CONTRIBUTING.md](CONTRIBUTING.md).
