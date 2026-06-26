# CodeWeave

[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Model Context Protocol](https://img.shields.io/badge/MCP-compatible-blue)](https://modelcontextprotocol.io/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Security](https://img.shields.io/badge/security-policy-green.svg)](SECURITY.md)

CodeWeave is a fast, local-first Model Context Protocol (MCP) server for AI-assisted software development. It gives ChatGPT Apps and Claude or Perplexity connectors controlled access to code search, exact file reads, transactional edits, Git operations, and approved development commands.

## Highlights

- **Single Rust process** â€” no Node.js gateway or companion daemon.
- **Repository-aware retrieval** â€” ranked context, symbols, references, outlines, regex, filename search, and repository maps.
- **Safe edits** â€” narrow single-operation tools with snapshot and content-hash preconditions, validation, and rollback.
- **Controlled execution** â€” allow-listed commands, optional task profiles, timeouts, and retained task logs.
- **Git integration** â€” status, diff, log, show, blame, staging, commits, and confirmed restores.
- **Dynamic workspaces** â€” switch repositories without restarting while keeping isolated per-repository caches.
- **Remote MCP** â€” expose the local server through ngrok, Cloudflare Tunnel, or another trusted HTTPS reverse proxy.

## Quick start

### 1. Install prerequisites

Install Git and Rust with [`rustup`](https://rustup.rs/). Platform-specific instructions are available in [docs/installation.md](docs/installation.md).

```bash
rustc --version
cargo --version
git --version
```

### 2. Build and configure

```bash
git clone <your-fork-or-repository-url>
cd codeweave
cp config.example.json config.json
cargo build --release
```

PowerShell:

```powershell
Copy-Item config.example.json config.json
cargo build --release
```

Edit `config.json` and replace the example workspace paths. Keep `allowedRoots` as narrow as practical.

`server.authMode` accepts only `bearer` or `none`. For HTTP transport with `bearer` enabled, CodeWeave automatically creates the configured token file on first startup if it does not exist. With the example configuration, this creates `.mcp-token` beside `config.json`. The file is already excluded by `.gitignore`.

This is an **HTTP origin bearer token**, not an LLM credential or MCP capability. It protects CodeWeave's local `/mcp` HTTP endpoint and is normally injected by a trusted tunnel or reverse proxy. Stdio transport does not read or create this token file.

To generate it manually instead:

PowerShell:

```powershell
$bytes = New-Object byte[] 32
[Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
[Convert]::ToHexString($bytes).ToLower() | Set-Content .mcp-token -NoNewline
```

macOS/Linux:

```bash
openssl rand -hex 32 > .mcp-token
chmod 600 .mcp-token
```

### 3. Start CodeWeave

Open **Terminal 1**:

```bash
cargo run --release -- --transport http --config config.json
```

CodeWeave starts at:

- MCP: `http://127.0.0.1:8820/mcp`
- Liveness: `http://127.0.0.1:8820/live`
- Health: `http://127.0.0.1:8820/health`

The server token in `.mcp-token` protects the local origin. It is an internal server-to-tunnel credential. You do **not** paste it into ChatGPT Apps, Claude connectors, or Perplexity connectors.

### 4. Expose CodeWeave over HTTPS

ChatGPT Apps and remote Claude or Perplexity connectors require a reachable HTTPS MCP URL. Start one tunnel in **Terminal 2**.

#### Option A: ngrok

Install ngrok and authenticate it once:

```bash
ngrok config add-authtoken <your-ngrok-authtoken>
```

PowerShell:

```powershell
.\start-ngrok.ps1 -Config .\config.json
```

The helper reads the configured port and token file, creates a temporary ngrok Traffic Policy, injects `Authorization: Bearer <token>` into forwarded requests, and removes the temporary policy when ngrok stops.

For a reserved ngrok domain:

```powershell
.\start-ngrok.ps1 -Config .\config.json -Domain mcp.example.ngrok.app
```

Use the HTTPS forwarding URL shown by ngrok and append `/mcp`:

```text
https://example.ngrok.app/mcp
```

The connector receives only the public URL. ngrok injects CodeWeaveâ€™s internal bearer token when forwarding requests to the local server.

#### Option B: Cloudflare Tunnel

A quick Cloudflare Tunnel can expose a local server with:

```bash
cloudflared tunnel --url http://127.0.0.1:8820
```

A basic quick tunnel does not inject CodeWeaveâ€™s internal `Authorization` header. For temporary testing, create a separate local config with:

```json
{
  "server": {
    "host": "127.0.0.1",
    "port": 8820,
    "authMode": "none"
  }
}
```

Then run CodeWeave with that temporary config and start `cloudflared` in the second terminal. Treat the generated URL as sensitive and stop the tunnel immediately after testing.

For a persistent Cloudflare deployment, keep CodeWeave bearer authentication enabled and place a Cloudflare Worker, authenticated reverse proxy, or equivalent trusted gateway in front of the origin to inject the internal bearer token. Do not publish an unauthenticated permanent endpoint.

#### Option C: another reverse proxy

Any HTTPS reverse proxy can be used when it:

1. accepts the public connector request;
2. forwards it to `http://127.0.0.1:8820/mcp`;
3. injects `Authorization: Bearer <contents of .mcp-token>` at the origin;
4. does not expose the token to the AI client.

## Connect an AI client

Once the tunnel is running, use its public HTTPS `/mcp` URL:

- **ChatGPT:** add CodeWeave through the **Apps** interface.
- **Claude:** add CodeWeave as a custom **Connector**.
- **Perplexity:** add CodeWeave as a custom **Connector** where supported.

No CodeWeave bearer token is entered in these client interfaces. Authentication to the local CodeWeave origin is handled internally by the tunnel or reverse proxy.

Detailed guides:

- [ChatGPT App setup](docs/connect-chatgpt.md)
- [Claude Connector setup](docs/connect-claude.md)
- [Perplexity Connector setup](docs/connect-perplexity.md)

## Configuration

```json
{
  "server": {
    "host": "127.0.0.1",
    "port": 8820,
    "authMode": "bearer",
    "tokenFile": ".mcp-token",
    "statefulMode": false,
    "jsonResponse": true
  },
  "workspace": {
    "defaultPath": "/path/to/projects/example",
    "allowedRoots": ["/path/to/projects"],
    "artifactPaths": ["artifacts"]
  },
  "skills": {
    "enabled": false,
    "roots": [],
    "explicitOnly": true
  },
  "policy": {
    "maxFileBytes": 2000000,
    "maxContextChars": 50000,
    "maxSearchResults": 100,
    "maxTaskOutputChars": 30000,
    "shellEnabled": false,
    "allowedCommands": ["git", "rg", "node", "npm", "npx", "pnpm", "python", "pytest", "cargo"]
  },
  "tasks": {
    "test": {
      "command": ["cargo", "test"],
      "timeoutMs": 120000,
      "outputFilter": { "type": "failedTail", "chars": 30000 }
    },
    "check": {
      "command": ["cargo", "check", "--message-format=json"],
      "timeoutMs": 120000,
      "background": true,
      "outputFilter": { "type": "cargoJson", "includeWarnings": true }
    }
  }
}
```

Task profiles can set `background: true` for long builds, browser smoke tests, and acceptance suites. Explicit `run` calls can override `background` and `timeout_ms`. Profile `outputFilter` values are:

- `{ "type": "raw" }` — successful tasks show the head; failed, cancelled, and timed-out tasks show the tail.
- `{ "type": "failedTail", "chars": 30000 }` — use a specific failure-tail budget.
- `{ "type": "tailLines", "lines": 40 }` — useful when a Python or Node script prints its summary last.
- `{ "type": "cargoJson", "includeWarnings": true }` — extracts Cargo compiler diagnostics from `--message-format=json`.
- `{ "type": "jsonSummary", "marker": "CODEWEAVE_SUMMARY:" }` — returns a script-emitted JSON summary after the marker.

Task output is written incrementally. While a background task is running, call `run` with `action: "status"` for its live tail or `action: "output"` with `stream: "combined"`, `"stdout"`, or `"stderr"`. Reuse the returned continuation token to page through the selected stream. Timeouts and cancellation retain partial logs. On Windows, task processes are assigned to a kill-on-close Job Object so descendant processes such as `rustc`, Node, and Chromium are cleaned up with the task.

`server.statefulMode` defaults to `false` and `server.jsonResponse` defaults to `true`, so Streamable HTTP uses direct POST responses without a persistent GET/SSE stream. Enable stateful mode only for MCP clients that require server-initiated messages or session-level SSE.

`workspace.allowedRoots` is a security boundary. CodeWeave canonicalizes requested repository paths and rejects paths outside those roots, including junction and symlink escapes.

Never commit `config.json`, `.mcp-token`, tunnel credentials, generated caches, or private repository paths.

## Tools

| Tool | Purpose |
| --- | --- |
| `workspace` | Open or switch the active repository, summarize state, refresh, and inspect session changes |
| `code_context` | Retrieve ranked semantic and syntax-aware context |
| `code_search` | Search text, regex, filenames, symbols, references, outlines, or the repository map |
| `code_fetch` | Read exact files, line ranges, symbols, handles, continuations, and task logs |
| `code_write` | Create or overwrite exactly one file |
| `code_replace` | Replace exact text in exactly one file |
| `code_insert` | Insert text relative to a named symbol in one file |
| `code_delete` | Delete exactly one file |
| `code_rename` | Rename exactly one file |
| `git` | Inspect and perform bounded Git operations |
| `run` | Run configured tasks or policy-approved commands |

A typical coding-agent workflow is:

1. Open an approved repository with `workspace`.
2. Use `code_context` for unfamiliar code.
3. Locate exact definitions with `code_search`.
4. Read only the required ranges with `code_fetch`.
5. Apply one scoped change with `code_write`, `code_replace`, `code_insert`, `code_delete`, or `code_rename`.
6. Run checks with `run`.
7. Review the final diff with `git`.

See [docs/implementation.md](docs/implementation.md) and [docs/tools.md](docs/tools.md).

## Security

CodeWeave can read and modify source code and run approved commands. Treat every connected App or Connector as a trusted coding agent.

The secure default is:

- bind CodeWeave to loopback;
- keep bearer authentication enabled at the local origin;
- inject that credential inside the tunnel or reverse proxy;
- restrict `allowedRoots`;
- leave shell execution disabled;
- keep the executable allow-list minimal;
- test first against a disposable or non-critical repository.

Read [SECURITY.md](SECURITY.md) before exposing CodeWeave beyond localhost.

## Development

```bash
cargo fmt --check
cargo test --release
cargo clippy --all-targets -- -D warnings
cargo build --release
```

See [CONTRIBUTING.md](CONTRIBUTING.md).

## Project status

CodeWeave is an early-stage project. APIs, configuration, and tool schemas may evolve before a stable 1.0 release.

## License

CodeWeave is licensed under the [MIT License](LICENSE).
