# Implementation Guide

## Deployment model

CodeWeave runs on the machine that contains the repositories an AI client is allowed to access.

```text
AI client -> MCP transport -> CodeWeave -> approved repositories and commands
```

Use stdio when a local client can launch the executable directly. Use HTTP when a client requires a URL. Stdio does not use HTTP authentication and therefore does not load or create the configured bearer-token file.

`server.authMode` accepts only `bearer` or `none`. The bearer token is an internal HTTP origin credential for CodeWeave's `/mcp` endpoint; it is not an LLM token, model feature, or credential that should be entered into an AI client.

## HTTP deployment

A safer HTTP deployment follows this pattern:

1. CodeWeave binds to loopback.
2. Bearer authentication protects `/mcp`.
3. A trusted HTTPS tunnel or reverse proxy exposes the endpoint when remote access is required.
4. The client sends requests to the HTTPS `/mcp` URL.
5. CodeWeave resolves repositories only inside `workspace.allowedRoots`.

Do not expose an unauthenticated CodeWeave endpoint to a public network.

## Workspace lifecycle

CodeWeave keeps one active repository per MCP session. Opening a different approved path first builds or reuses the cached repository actor and only swaps that session after opening succeeds, so a failed switch leaves the session's current repository usable. Persistent indexes and cached actors remain separated by canonical path under `.codeweave-cache`.

Repository switching is explicit through `workspace(action="open", path="...")`. A switch is rejected while tasks are running in that session's active repository because task status and cancellation belong to the cached repository actor. Stateful Streamable HTTP is recommended for independent chats; stateless HTTP shares one fallback workspace key.

## Editing model

CodeWeave exposes narrow write tools:

- `code_write` for one whole-file write;
- `code_replace` for one exact replacement;
- `code_insert` for one symbol-relative insertion;
- `code_delete` for one deletion;
- `code_rename` for one rename.

For coordinated changes, `code_preview` accepts a `changes` array and returns the planned diff without writing files. `code_transaction` accepts the same `changes` array and applies it through the same edit engine.

The internal edit pipeline plans changes, checks preconditions, runs syntax preflight, writes atomically, records mutations, runs optional validation profiles, and restores the prior state when validation fails.

Existing-file changes require a current snapshot, expected content hash, or provenance handle.

Tool errors include the stable `code` and `message` fields plus retry metadata when recovery is possible. `retry_kind` distinguishes `retry_same_request`, `retry_with_changes`, and `not_retryable`; argument-correctable failures may include `suggested_calls`.

## Command execution

The `run` tool executes configured task profiles or executables listed in `policy.allowedCommands`. Configured profiles are trusted server configuration and may resolve explicit repository-local executable paths such as a virtual-environment Python. Ad-hoc `command` requests still require `policy.allowedCommands`. Shell execution is disabled by default. Keep task working directories relative to the active workspace and avoid adding broad command interpreters unless required.

If a hosted client blocks `run(action="status")` because the same tool can also start commands, use the `status_fetch` descriptor returned when the task starts. It maps to `code_fetch` with `{"kind":"task_status","value":"task_..."}` and provides a read-only polling path that hosted safety classifiers can distinguish from command execution.

## Recommended agent workflow

1. Open the repository.
2. Retrieve ranked context, using required, optional, excluded, and document-type filters for broad scopes.
3. Search for precise definitions and references; regex is raw text search, while `references` is the symbol call-site mode.
4. Fetch exact file ranges, metadata, or compact responses as needed.
5. Preview multi-file edits when useful, then apply the smallest coherent edit.
6. Run formatting, tests, and builds.
7. Inspect Git status and diff.
8. Commit only after human review.

## Production checklist

- release build completed;
- formatting, Clippy, and tests pass;
- local configuration is ignored by Git;
- unique bearer token generated;
- `allowedRoots` reviewed;
- command allow-list reviewed;
- HTTPS configured for remote access;
- logs checked for secrets or private paths;
- a clean Git state or backup is available;
- client tested first against a non-critical repository.
