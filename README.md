# CodeWeave

CodeWeave is a local-first MCP server for deterministic code retrieval, safe
editing, Git operations, and Bash execution against one repository.

## Requirements

- Rust stable when building from source
- Git and a valid Git worktree
- Bash (Git Bash is supported on Windows)
- Optional language servers: rust-analyzer, basedpyright, and
  typescript-language-server

## Start

```bash
cargo build --release
./target/release/codeweave-rust init --config config.json --path /absolute/path/to/repo
./target/release/codeweave-rust doctor --config config.json
./target/release/codeweave-rust --config config.json --transport stdio
```

For streamable HTTP:

```bash
./target/release/codeweave-rust --config config.json --transport http
```

HTTP is stateless, uses JSON responses, and normally binds to loopback with a
bearer token. The generated token file is created exclusively and with mode
`0600` on Unix. Use a trusted TLS tunnel or reverse proxy for remote clients.

## Configuration

Configuration requires `configVersion: 2` and rejects unknown fields,
including unknown nested fields. See [config.example.json](config.example.json).
Git and Bash are validated before the server accepts requests.

## MCP tools

CodeWeave exposes one fixed 25-tool surface:

- `workspace`, `code_retrieve`, `code_intelligence`
- `code_write`, `code_replace`, `code_replace_range`, `code_insert`,
  `code_delete`, `code_rename`, `code_preview`, `code_transaction`
- `git_status`, `git_diff`, `git_log`, `git_show`, `git_blame`,
  `git_preflight`, `git_stage`, `git_commit`, `git_restore`, `git_push`
- `bash`, `bash_status`, `bash_output`, `bash_cancel`

All clients connected to one process share workspace mutations, generations,
and Bash runs. `workspace.changes` is bounded process-local history; Git is the
durable record. Restarting creates a new `instance_id` and resets generation.

## Transaction guarantees

Edits enforce snapshot/file preconditions and replace each file atomically.
Multi-file transactions use preflight plus best-effort compensation, but do not
claim cross-file atomicity. Partial commits and restoration failures are
reported explicitly. Validation never silently rolls back an applied edit.

Architecture and concurrency invariants are documented in
[docs/architecture.md](docs/architecture.md). Client setup is covered under
`docs/connect-chatgpt.md` and `docs/connect-claude.md`.
