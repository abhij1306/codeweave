# CodeWeave

[![Rust](https://img.shields.io/badge/Rust-1.80%2B-orange?logo=rust)](https://www.rust-lang.org/)
[![Model Context Protocol](https://img.shields.io/badge/MCP-compatible-blue)](https://modelcontextprotocol.io/)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)
[![Security](https://img.shields.io/badge/security-policy-green.svg)](SECURITY.md)

CodeWeave is a fast, local-first Model Context Protocol (MCP) server for AI-assisted software development. It gives ChatGPT Apps and Claude connectors controlled access to code search, exact file reads, transactional edits, Git operations, and approved development commands.

## Highlights

- **Single Rust process** — no Node.js gateway or companion daemon.
- **Repository-aware retrieval** — ranked context, symbols, references, outlines, regex, filename search, and repository maps.
- **Optional semantic intelligence** — persistent Python and TypeScript language servers for definitions, references, diagnostics, and safe rename previews, with tree-sitter/lexical fallback.
- **Safe edits** — narrow single-operation tools with snapshot and content-hash preconditions, non-destructive validation reporting, and atomic recovery for internal write failures.
- **Supervised Bash execution** — focused commands, timeouts, cancellation, retained logs, and process-tree cleanup.
- **Git integration** — status, diff, log, show, blame, staging, commits, and confirmed restores.
- **Single-repository focus** — one instance serves exactly one repository, configured through `workspace.path` and fixed for the process lifetime. The index and file watcher are eager (ready before the transport binds). Run two projects as two instances on two ports.
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
cargo build --release
```

PowerShell:

```powershell
cargo build --release
```

For a first-time setup, `init` creates this file, sets the project path, and creates the bearer token for you:

```bash
cargo run -- init --path /absolute/path/to/project
cargo run -- doctor --config config.json
```

PowerShell:

```powershell
cargo run -- init --path C:\Development\project
cargo run -- doctor --config config.json
```

`doctor` is the pre-flight check: it validates the configuration, project path, Git, port, token, index, and Bash availability. It exits non-zero when an actionable check fails. You can still copy the example and edit `workspace.path` manually if preferred.

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

The explicit equivalent is:

```bash
cargo run --release -- serve --transport http --config config.json
```

The original bare invocation remains supported.

CodeWeave starts at:

- MCP: `http://127.0.0.1:8813/mcp`
- Liveness: `http://127.0.0.1:8813/live`
- Health: `http://127.0.0.1:8813/health`

The server token in `.mcp-token` protects the local origin. It is an internal server-to-tunnel credential. You do **not** paste it into ChatGPT Apps or Claude connectors.

### 4. Expose CodeWeave over HTTPS

ChatGPT Apps and remote Claude connectors require a reachable HTTPS MCP URL. Start one tunnel in **Terminal 2**.

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
cloudflared tunnel --url http://127.0.0.1:8813
```

A basic quick tunnel does not inject CodeWeave’s internal `Authorization` header. For temporary testing, create a separate local config with:

```json
{
  "server": {
    "host": "127.0.0.1",
    "port": 8813,
    "authMode": "none"
  }
}
```

Then run CodeWeave with that temporary config and start `cloudflared` in the second terminal. Treat the generated URL as sensitive and stop the tunnel immediately after testing.

For a persistent Cloudflare deployment, keep CodeWeave bearer authentication enabled and place a Cloudflare Worker, authenticated reverse proxy, or equivalent trusted gateway in front of the origin to inject the internal bearer token. Do not publish an unauthenticated permanent endpoint.

#### Option C: another reverse proxy

Any HTTPS reverse proxy can be used when it:

1. accepts the public connector request;
2. forwards it to `http://127.0.0.1:8813/mcp`;
3. injects `Authorization: Bearer <contents of .mcp-token>` at the origin;
4. does not expose the token to the AI client.

## Connect an AI client

Once the tunnel is running, use its public HTTPS `/mcp` URL:

- **ChatGPT:** add CodeWeave through the **Apps** interface.
- **Claude:** add CodeWeave as a custom **Connector**.

No CodeWeave bearer token is entered in these client interfaces. Authentication to the local CodeWeave origin is handled internally by the tunnel or reverse proxy.

Detailed guides:

- [ChatGPT App setup](docs/connect-chatgpt.md)
- [Claude Connector setup](docs/connect-claude.md)

## Configuration

```json
{
  "server": {
    "host": "127.0.0.1",
    "port": 8813,
    "authMode": "bearer",
    "tokenFile": ".mcp-token",
    "statefulMode": false,
    "jsonResponse": true,
    "idleTimeoutMs": 5000,
    "toolProfile": "full",
    "allowedHosts": []
  },
  "workspace": {
    "path": "/path/to/projects/example",
    "artifactPaths": ["artifacts"],
    "excludePaths": ["**/__pycache__/", "**/.pytest_cache/", "**/.mypy_cache/", "**/.ruff_cache/", "*.log"]
  },
  "skills": {
    "enabled": false,
    "roots": [],
    "explicitOnly": true
  },
  "intelligence": {
    "python": {"enabled": false, "command": "basedpyright-langserver", "args": ["--stdio"], "timeoutMs": 10000},
    "typescript": {"enabled": false, "command": "typescript-language-server", "args": ["--stdio"], "timeoutMs": 10000}
  },
  "policy": {
    "maxFileBytes": 2000000,
    "maxContextChars": 50000,
    "maxSearchResults": 100,
    "bash": {
      "enabled": true,
      "executable": "bash",
      "defaultTimeoutMs": 120000,
      "maxTimeoutMs": 300000,
      "maxOutputChars": 30000,
      "retentionHours": 1
    }
  }
}
```

`workspace.excludePaths` accepts workspace-relative gitignore-style exclusion patterns. Excluded paths are omitted from indexing, filesystem-watcher reconciliation, and workspace change summaries. Add repository-specific generated directories such as `backend/artifacts/` when they do not contain source fixtures. Negated (`!`) reinclusion patterns are not supported.

`workspace.artifactPaths` has the opposite purpose: it explicitly indexes configured paths even when normal Git ignore rules would skip them. Do not list the same directory in both settings.

`code_retrieve` is the single public repository retrieval surface. The calling agent selects explicit operations for filename discovery, symbols, text search, references, outlines, repository maps, or exact reads. CodeWeave performs those operations deterministically and can batch up to 12 of them in one call.

`intelligence` configures optional persistent language servers. Disabled adapters retain tree-sitter and lexical behavior. Enabled adapters start lazily on the first `code_intelligence` request and restart once after failure. CodeWeave never installs these executables automatically; run `codeweave doctor --config config.json` after changing their paths.

`bash` executes a command string through the configured executable as `bash -c <command>`. For example, a focused Windows repository test can run as `cd backend && ./.venv/Scripts/python.exe -m pytest tests/unit/test_file.py -q`. `cwd` may select an existing workspace-relative directory, `background` returns immediately with a `run_id`, and `timeout_ms` may override the configured default up to `maxTimeoutMs`.

Bash output is written incrementally. Use `bash_status` for the live tail or `bash_output` with `stream: "combined"`, `"stdout"`, or `"stderr"`; pass the returned continuation token to page through a stream. `bash_cancel` and timeouts retain partial logs. On Windows, runs are assigned to a kill-on-close Job Object so descendant processes such as `rustc`, Node, and Chromium are cleaned up with the run.

This execution surface is trusted-client functionality, not a sandbox. File tools and the `cwd` argument are constrained to the configured repository, but a Bash command can access anything available to the operating-system account running CodeWeave. CodeWeave validates Bash with a cheap `bash -c` readiness probe before reporting it available. On Windows, an explicit absolute `policy.bash.executable` wins; otherwise CodeWeave probes the configured executable, discovers Git for Windows Bash from `PATH`, common Git install locations, and the Git executable location, and only uses WSL when that configured/probed executable actually passes readiness.

`server.statefulMode: false` with `server.jsonResponse: true` is the recommended fast path. CodeWeave always serves the single repository configured under `workspace.path`; `statefulMode` only controls transport session isolation for Bash runs and per-chat `changes` attribution, never which repository is served.

`server.toolProfile` selects which tools the server advertises and accepts. It is resolved once at startup from a single tool registry that is the sole source of truth for every advertised tool:

- `full` (default) — all 26 tools.
- `read-only` — read/search/inspect only, including `workspace`, `code_retrieve`, `code_capabilities`, `code_intelligence`, `code_preview`, and the read-only git tools. No writes, no bash.
- `edit` — read plus in-repo writes (`code_write`/`code_replace`/…/`code_transaction`, `git_stage`/`git_commit`/`git_restore`), but no `bash` and no network-facing `git_push`.
- `custom` — start from the full set and refine with `"tools": { "include": [...], "exclude": [...] }`. A non-empty `include` is an allowlist; `exclude` subtracts. Unknown tool names fail startup.

A tool that exists but is not in the active profile returns a structured `TOOL_NOT_IN_PROFILE` error rather than being reported as unknown. Because edit `validate` commands run through bash, an edit that carries `validate` under a bash-free profile (or with `policy.bash.enabled: false`) is rejected up front with `VALIDATE_UNAVAILABLE` instead of silently skipping validation.

`server.idleTimeoutMs` defaults to `5000` and bounds how long an **idle keep-alive TCP connection** stays open. This is independent of request latency: even when every `POST /mcp` returns in milliseconds, Hyper keeps the underlying socket open for reuse, so a tunnel or connector (ngrok, the OpenAI connector) holds it until its own deadline — often ~90 seconds — and reports that as the **Connections** p50/p90, not the request duration. CodeWeave applies `idleTimeoutMs` as Hyper's `header_read_timeout` (the equivalent of Uvicorn's `timeout_keep_alive`, which is why Serena's dashboard reads ~5s): an idle connection is closed after the timeout, while an in-flight request — including a long foreground `bash` POST — is never interrupted, because the timeout resets per request. Set it to `0` to disable the bound and keep connections open until the peer closes them.

Use `server.statefulMode: true` only when multiple independent chats need isolated Bash runs and per-chat `changes` attribution in one server process; it does not change which repository is served. That mode gets isolation from RMCP session ids, but RMCP 1.8 serves stateful requests over long-lived SSE rather than direct JSON, so tunnel dashboards can show higher connection lifetimes even when CodeWeave tool `elapsed_ms` values are low. The public `GET /live` endpoint reports only non-sensitive service and transport status (`statefulMode`, `jsonResponse`, `idleTimeoutMs`, RMCP, and version metadata); authentication mode, workspace configuration, and build provenance are intentionally omitted.

`server.allowedHosts` extends rmcp's Host-header validation. Set it to exact public hostnames for fixed domains, or to `["*"]` for trusted local tunnels such as random ngrok URLs where bearer auth is injected before requests reach CodeWeave.

The configured `workspace.path` is the boundary for file tools. CodeWeave canonicalizes it once at startup and rejects file operations that resolve outside it, including junction and symlink escapes. It is not a boundary for commands run through `bash`.

Never commit `config.json`, `.mcp-token`, tunnel credentials, generated caches, or private repository paths.

## Tools

| Tool | Purpose |
| --- | --- |
| `workspace` | Summarize the configured repository, refresh its index, report diagnostics, and inspect session changes |
| `code_retrieve` | Discover and read repository evidence with explicit batched operations |
| `code_capabilities` | Inspect retrieval, intelligence, editing, execution, and limit contracts |
| `code_preview` | Preview a multi-file edit transaction and return the diff without writing files |
| `code_transaction` | Apply a multi-file edit transaction with preconditions, non-destructive validation reporting, diff output, and atomic internal recovery |
| `code_write` | Create or overwrite exactly one file |
| `code_replace` | Replace exact text in exactly one file |
| `code_replace_range` | Replace the complete line range selected by a retrieval handle |
| `code_insert` | Insert text relative to a named symbol in one file |
| `code_delete` | Delete exactly one file |
| `code_rename` | Rename exactly one file |
| `git_status`, `git_diff`, `git_log`, `git_show`, `git_blame`, `git_preflight` | Inspect repository state without mutation |
| `git_stage`, `git_commit` | Update the local Git index or create a commit |
| `git_restore` | Restore selected paths after explicit confirmation |
| `git_push` | Push commits to a remote after explicit confirmation (`confirm: true`) |
| `bash` | Run a Bash command in the active workspace |
| `bash_status`, `bash_output` | Read retained Bash run state and output |
| `bash_cancel` | Cancel a running background Bash run |

A typical coding-agent workflow is:

1. Open the configured repository with `workspace` and inspect `code_capabilities` when needed.
2. Decompose the task in the calling agent, then issue one `code_retrieve` batch containing explicit `find_file`, `find_symbol`, `search_text`, `find_references`, `symbols_overview`, `repo_map`, or `read` operations.
3. Use `read` operations for exact files, ranges, symbols, metadata, handles, continuations, or retained Bash output.
4. Use `code_intelligence` only when semantic definition, reference, diagnostic, or rename-preview evidence is required.
5. Preview multi-file edits with `code_preview`, then apply with `code_transaction` or a narrow write wrapper.
6. Run focused checks with `bash` and inspect retained output with `bash_status` or `bash_output`.
7. Review the final state with `git_status` and `git_diff`.

See [docs/implementation.md](docs/implementation.md) and [docs/tools.md](docs/tools.md).
## Security

CodeWeave can read and modify source code and run arbitrary Bash commands as its operating-system user. Treat every connected App or Connector as a trusted coding agent.

The secure default is:

- bind CodeWeave to loopback;
- keep bearer authentication enabled at the local origin;
- inject that credential inside the tunnel or reverse proxy;
- point `workspace.path` at the single repository this instance should serve;
- disable `policy.bash.enabled` when command execution is not required;
- run CodeWeave under a dedicated, least-privileged operating-system account;
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
