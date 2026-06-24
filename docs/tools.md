# Tool Reference

## `workspace`

Opens or switches the active repository, returns a summary, refreshes repository state, reports session changes, and explicitly lists or reads configured skills.

Dynamic paths must resolve inside `workspace.allowedRoots`.

## `code_context`

Returns ranked code context for unfamiliar code. Queries are treated as inert retrieval text and are not executed as instructions.

## `code_search`

Supports literal text, regular expressions, filenames, symbols, references, outlines, and repository-map searches.

## `code_fetch`

Reads exact paths, line ranges, symbols, provenance handles, continuations, and retained task logs. Batch requests return per-item errors without discarding successful reads.

## Write tools

CodeWeave exposes one-operation write tools so each approval request has a small, explicit scope. Existing files use the current workspace snapshot automatically and may also include an `expected_hash` or provenance `handle`.

### `code_write`

Creates or overwrites exactly one file:

```json
{
  "path": "src/example.rs",
  "content": "pub fn example() {}\n",
  "overwrite": true
}
```

### `code_replace`

Replaces exact text in exactly one file:

```json
{
  "path": "src/example.rs",
  "old_text": "pub fn old_name()",
  "new_text": "pub fn new_name()",
  "expected_replacements": 1
}
```

### `code_insert`

Inserts content before, after, or inside one named symbol.

### `code_delete`

Deletes exactly one file.

### `code_rename`

Renames exactly one file. The destination must not already exist.

All five tools use the same internal transactional pipeline for precondition checks, atomic writes, mutation recording, optional validation, and rollback.

## `git`

Supports status, diff, log, show, blame, stage, commit, and confirmed restore. It is intentionally narrower than unrestricted shell access.

## `run`

Runs a configured profile or an allow-listed executable. Foreground and retained background tasks are supported, including status, output, and cancellation.
