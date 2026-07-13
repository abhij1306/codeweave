# Tool Profile Evaluation

**Date:** 2026-07-13  
**Repository revision:** `d749c345c3911b19c90065fc66e46c479a125982`  
**Clients evaluated:** ChatGPT web and Claude with the live CodeWeave connector  
**Claude status:** external connector workflow passed

## Decision

The non-default `coding` profile passes both ChatGPT and Claude coding workflows. Keep `full` as the default for this release; any default change must still be made explicitly in a separate versioned release.

The evaluated coding surface is:

```text
workspace
code_retrieve
code_intelligence
code_write
code_replace
code_replace_range
code_insert
code_delete
code_rename
code_preview
code_transaction
git_status
git_diff
git_log
bash
bash_status
bash_output
bash_cancel
```

These remain `full`-only:

```text
code_capabilities
git_show
git_blame
git_preflight
git_stage
git_commit
git_restore
git_push
```

`custom` remains unchanged. `read-only` and `edit` remain compatibility profiles with smaller bounded surfaces.

## ChatGPT workflow result

The live connector workflow exercised:

- workspace summary;
- batched retrieval, exact reads, symbol discovery, and outlines;
- semantic reference routing and fallback;
- every narrow edit primitive;
- handle-bound complete-range replacement;
- multi-file preview and transaction;
- post-apply validation commands;
- background Bash start, status, paged output, cancellation, and retained partial output;
- Git status, diff/log inspection, and preflight inspection;
- complete cleanup of temporary `.ai-bridge` probe files.

Results:

| Metric | Result |
|---|---:|
| Workflow task failures | 0 |
| Malformed calls in the reversible coding workflow | 0 |
| Edit primitive failures | 0 |
| Validation failures | 0 |
| Temporary files left behind | 0 |
| Probe initial/final tracked state | Equivalent before profile implementation |

A later audit-only lookup made one correctable malformed `find_file` operation by sending `pattern` instead of the documented `name` field. The server returned the expected structured `UNKNOWN_OPERATION_FIELD` error, and the corrected request succeeded. This is recorded separately from the fixed reversible workflow so connector comparisons use the same prompt boundary.

The configured Rust language server timed out during a reference request. CodeWeave returned a current lexical/syntactic fallback with `fallback_reason.code = LSP_TIMEOUT`; it did not mislabel fallback output as semantic.

## Claude workflow result

Claude completed the same reversible coding workflow with 26 tool calls, zero abandoned tasks, two successful retries, and an initial/final Git status that matched path-for-path and state-for-state. It exercised retrieval, intelligence fallback, create/replace/insert/range-replace/rename, preview, transaction, Bash supervision, Git inspection, cleanup, and final absence checks.

One retry followed a client-side attempt to invoke `git_log` before Claude had loaded its schema through `tool_search`. The second followed an intentionally enforced `STALE_FILE` precondition after a transaction changed the renamed file's hash. Neither required a tool outside the `coding` profile. Semantic fallback remained honestly labelled `fallback` with `LSP_TIMEOUT`, and no temporary files remained.

The server's production `tools/list` independently confirmed all 18 coding tools, including `code_write`. Claude's on-demand tool search surfaced 17 CodeWeave tools and did not select `code_write`; it used `code_transaction` with `kind: "create"` instead. This is recorded as a Claude discovery-layer caveat, not a missing server capability. An unrelated Gmail tool surfaced by Claude is excluded from CodeWeave profile counts. Full-only tools were not observed or needed.

## Schema footprint

Serialized `tools/list` payloads measured by deterministic registry tests:

| Profile | Tools | Bytes |
|---|---:|---:|
| `full` | 26 | 24,418 |
| `coding` | 18 | 20,174 |
| `edit` | 14 | 17,060 |
| `read-only` | 7 | 8,620 |

`coding` removes 4,244 bytes from the full payload, a **17.4% reduction**, while retaining every tool used by the live coding workflow.

## Configuration boot matrix

Temporary configs for `full`, `coding`, `read-only`, `edit`, and `custom` were passed through the real `doctor` startup path. Every profile parsed, resolved, initialized the workspace and index, validated Git and language-server configuration, and completed successfully. Registry tests separately verify advertised Bash availability because `doctor` reports operating-system executable readiness independently of profile visibility.

A production stdio MCP process was also initialized with `toolProfile: coding`; its live `tools/list` response contained exactly the expected 18 tools in registry order and none of the full-only tools.

## Release decision

The cross-client Phase 6 gate is complete: both ChatGPT and Claude finished the reversible workflow without an abandoned task or a need for a full-only tool. The `coding` profile remains non-default in this release. Changing the default from `full` to `coding` requires a separate versioned release and an explicit compatibility decision; it is not part of this implementation slice.
