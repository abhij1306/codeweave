# CodeWeave

[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Model Context Protocol](https://img.shields.io/badge/MCP-compatible-blue)](https://modelcontextprotocol.io/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Security](https://img.shields.io/badge/security-policy-green.svg)](SECURITY.md)

CodeWeave is a fast, local-first Model Context Protocol (MCP) server for AI-assisted software development. It gives ChatGPT Apps and Claude or Perplexity connectors controlled access to code search, exact file reads, transactional edits, Git operations, and approved development commands.

## Highlights

- **Single Rust process** — no Node.js gateway or companion daemon.
- **Repository-aware retrieval** — ranked context, symbols, references, outlines, regex, filename search, and repository maps.
- **Safe edits** — narrow single-operation tools with snapshot and content-hash preconditions, validation, and rollback.
- **Controlled execution** — configured task profiles, timeouts, cancellation, and retained task logs.
- **Git integration** — status, diff, log, show, blame, staging, commits, and confirmed restores.
- **Session-isolated dynamic workspaces** — each MCP session can switch repositories without restarting while cached repository actors are reused by canonical path.
- **Remote MCP** — expose the local server through ngrok, Cloudflare Tunnel, or another trusted HTTPS reverse proxy.

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

The helper reads the configured port and token file, creates a temporary ngrok Traffic Policy, injects `Authorization: Bearer <token>` into forwarded requests, and removes the temporary policy when ngrok stops. For random ngrok URLs, keep `server.allowedHosts` set to `["*"]`; CodeWeave still requires the injected bearer token, and this avoids 403 responses from MCP Host validation when the public ngrok hostname changes.

For a reserved ngrok domain:

```powershell
.\start-ngrok.ps1 -Config .\config.json -Domain mcp.example.ngrok.app
```

Use the HTTPS forwarding URL shown by ngrok and append `/mcp`:

```text
https://example.ngrok.app/mcp
```

The connector receives only the public URL. ngrok injects CodeWeave’s internal bearer token when forwarding requests to the local server.

#### Option B: Cloudflare Tunnel

A quick Cloudflare Tunnel can expose a local server with:

```bash
cloudflared tunnel --url http://127.0.0.1:8820
```

A basic quick tunnel does not inject CodeWeave’s internal `Authorization` header. For temporary testing, create a separate local config with:

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
    "statefulMode": true,
    "jsonResponse": false,
    "allowedHosts": ["*"]
  },
  "workspace": {
    "defaultPath": "/path/to/projects/example",
    "allowedRoots": ["/path/to/projects"],
    "artifactPaths": ["artifacts"],
    "excludePaths": ["**/__pycache__/", "**/.pytest_cache/", "**/.mypy_cache/", "**/.ruff_cache/", "*.log"]
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

`workspace.excludePaths` accepts workspace-relative gitignore-style exclusion patterns. Excluded paths are omitted from indexing, filesystem-watcher reconciliation, and workspace change summaries. Add repository-specific generated directories such as `backend/artifacts/` when they do not contain source fixtures. Negated (`!`) reinclusion patterns are not supported.

`workspace.artifactPaths` has the opposite purpose: it explicitly indexes configured paths even when normal Git ignore rules would skip them. Do not list the same directory in both settings. Configured entries under `workspaces` may define their own `artifactPaths` and `excludePaths`; dynamically opened repositories inherit the values under `workspace`.

`task_run` accepts a configured profile name only. Profiles can set `background: true` and `timeoutMs` for long builds, browser smoke tests, and acceptance suites. The example configuration includes `vp-check`, `vp-test`, and `vp-build` profiles for CrawlerAI's Vite+ frontend; these intentionally use the required `vp` CLI rather than `npm`. Profile `outputFilter` values are:

- `{ "type": "raw" }` - successful tasks show the head; failed, cancelled, and timed-out tasks show the tail.
- `{ "type": "failedTail", "chars": 30000 }` - use a specific failure-tail budget.
- `{ "type": "tailLines", "lines": 40 }` - useful when a Python or Node script prints its summary last.
- `{ "type": "cargoJson", "includeWarnings": true }` - extracts Cargo compiler diagnostics from `--message-format=json`.
- `{ "type": "jsonSummary", "marker": "CODEWEAVE_SUMMARY:" }` - returns a script-emitted JSON summary after the marker.

Task output is written incrementally. While a background task is running, call `task_status` for its live tail or `task_output` with `stream: "combined"`, `"stdout"`, or `"stderr"`. Reuse the returned continuation token to page through the selected stream. `task_cancel` and timeouts retain partial logs. On Windows, task processes are assigned to a kill-on-close Job Object so descendant processes such as `rustc`, Node, and Chromium are cleaned up with the task.

`server.statefulMode` defaults to `true` so independent chats or LLM clients receive isolated active-workspace state through the MCP session id. Stateful streamable HTTP uses long-lived SSE requests; ngrok's p50/p90 dashboard can include those request durations, so it may show high values even when tool responses report low `elapsed_ms`. `server.jsonResponse` defaults to `false` and only applies when `statefulMode` is disabled. Stateless HTTP remains supported for legacy direct JSON responses, but all stateless requests share one fallback workspace key.

`server.allowedHosts` extends rmcp's Host-header validation. Set it to exact public hostnames for fixed domains, or to `["*"]` for trusted local tunnels such as random ngrok URLs where bearer auth is injected before requests reach CodeWeave.

`workspace.allowedRoots` is a security boundary. CodeWeave canonicalizes requested repository paths and rejects paths outside those roots, including junction and symlink escapes.

Never commit `config.json`, `.mcp-token`, tunnel credentials, generated caches, or private repository paths.

## Tools

| Tool | Purpose |
| --- | --- |
| `workspace` | Open or switch this MCP session's active repository, summarize state, refresh, diagnostics, and inspect session changes |
| `code_context` | Retrieve ranked semantic and syntax-aware context |
| `code_capabilities` | Inspect supported search modes, fetch kinds, edit capabilities, limits, and known limitations |
| `code_search` | Search text, regex, filenames, symbols, references, outlines, or the repository map |
| `code_fetch` | Read exact files, line ranges, symbols, handles, continuations, task status, and task logs |
| `code_preview` | Preview a multi-file edit transaction and return the diff without writing files |
| `code_transaction` | Apply a multi-file edit transaction with preconditions, validation, diff output, and rollback |
| `code_write` | Create or overwrite exactly one file |
| `code_replace` | Replace exact text in exactly one file |
| `code_replace_range` | Replace the complete line range selected by a fetch handle |
| `code_insert` | Insert text relative to a named symbol in one file |
| `code_delete` | Delete exactly one file |
| `code_rename` | Rename exactly one file |
| `git_status`, `git_diff`, `git_log`, `git_show`, `git_blame`, `git_preflight` | Inspect repository state without mutation |
| `git_stage`, `git_commit` | Update the local Git index or create a commit |
| `git_restore` | Restore selected paths after explicit confirmation |
| `git_push` | Push commits to a remote |
| `task_run` | Run a configured task profile |
| `task_status`, `task_output` | Read retained background-task state and output |
| `task_cancel` | Cancel a running background task |

A typical coding-agent workflow is:

1. Open an approved repository with `workspace`.
2. Use `code_context` for unfamiliar code.
3. Locate exact definitions with `code_search`; use `references` for indexed symbol call-site discovery rather than regex.
4. Read only the required ranges with `code_fetch`; use `response_detail: "compact"` or `metadata` items when full debug fields or content are unnecessary.
5. Preview multi-file edits with `code_preview`, then apply with `code_transaction` or a narrow write wrapper.
6. Run configured checks with `task_run` and inspect retained output with `task_status` or `task_output`.
7. Review the final state with `git_status` and `git_diff`.

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
