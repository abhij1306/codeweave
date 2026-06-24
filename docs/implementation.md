# Implementation Guide

## Deployment model

CodeWeave runs on the machine that contains the repositories an AI client is allowed to access.

```text
AI client -> MCP transport -> CodeWeave -> approved repositories and commands
```

Use stdio when a local client can launch the executable directly. Use HTTP when a client requires a URL.

## HTTP deployment

A safer HTTP deployment follows this pattern:

1. CodeWeave binds to loopback.
2. Bearer authentication protects `/mcp`.
3. A trusted HTTPS tunnel or reverse proxy exposes the endpoint when remote access is required.
4. The client sends requests to the HTTPS `/mcp` URL.
5. CodeWeave resolves repositories only inside `workspace.allowedRoots`.

Do not expose an unauthenticated CodeWeave endpoint to a public network.

## Workspace lifecycle

CodeWeave keeps one active repository per MCP manager session. Opening a different approved path drops the previous in-memory actor and watcher, then opens the requested repository. Persistent indexes are separated by canonical path under `.codeweave-cache`.

Dynamic repository IDs are derived from canonical paths and recorded in the cache so later calls can resolve the correct repository without confusing it with another workspace.

## Editing model

CodeWeave exposes narrow write tools:

- `code_write` for one whole-file write;
- `code_replace` for one exact replacement;
- `code_insert` for one symbol-relative insertion;
- `code_delete` for one deletion;
- `code_rename` for one rename.

Each public call changes exactly one file operation. The internal edit pipeline still plans the change, checks preconditions, writes atomically, records mutations, runs optional validation profiles, and restores the prior state when validation fails.

Existing-file changes require a current snapshot, expected content hash, or provenance handle.

## Command execution

The `run` tool executes configured task profiles or executables listed in `policy.allowedCommands`. Shell execution is disabled by default. Keep task working directories relative to the active workspace and avoid adding broad command interpreters unless required.

## Recommended agent workflow

1. Open the repository.
2. Retrieve ranked context.
3. Search for precise definitions and references.
4. Fetch exact file ranges.
5. Apply the smallest coherent edit.
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
