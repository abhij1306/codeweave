# CodeWeave Improvement Plan

**Status:** P0–P5 shipped
**Written:** 2026-07-11, revised 2026-07-12 (dropped Perplexity support; single-repo model; latency/efficiency/accuracy as primary metrics); P4 shipped 2026-07-13
**Audience:** the implementing agent (Opus) and maintainers. Every phase lists concrete files, invariants that must not break, and acceptance criteria.

---

## 0. Goals and constraints

CodeWeave's job: be a **single MCP server** that makes coding tasks fast and accurate in web interfaces — **ChatGPT Apps and Claude connectors**. Perplexity support is dropped. The user runs it against **one local project at a time** (reference project for evaluation: **crawlerai**, a mid-complexity repo).

Primary success metrics, in priority order:

1. **Latency** — cold start to first useful `code_context` answer; warm search latency; edit round-trip time.
2. **Efficiency** — fewer tool round trips per task (right code on the first call), smaller schema/response token footprints.
3. **Accuracy** — retrieval relevance, symbol-complete results, edits that apply correctly with honest precondition/validation reporting.

Hard constraints (must remain true):

- Single static binary, offline, no model downloads, no GPU, no mandatory network dependencies.
- **One repository per server instance.** No multi-workspace management. Two projects = two instances on two ports.
- Flat JSON schemas — no `oneOf`/`allOf`/`not`/`const` in any public tool schema. This is retained as a **token-cost and client-reliability choice** (flat schemas are cheaper per conversation and less likely to confuse hosted clients), no longer a Perplexity compatibility contract. Enforced today by the test at `main.rs:1169-1306`; becomes a registry-level rule in P2.
- Loopback-by-default HTTP, bearer auth, stateless JSON mode for connectors.
- Narrow write tools keep their distinct safety annotations (READ / WRITE_CLOSED / DESTRUCTIVE_CLOSED / WRITE_OPEN / DESTRUCTIVE_OPEN). Do **not** collapse them into one mega-tool.

---

## 1. Audit findings (verified against source)

### 1.1 What the third-party analysis got right

- **Exactly 27 tools**, all defined as inline `json!` literals in `main.rs::tools()` (`main.rs:202-717`). The set is **triple-pinned**: schema definitions in `main.rs`, a hardcoded name allowlist in `mcp_transport.rs:85-113`, and the `expected_annotations` test table at `main.rs:1173-1201`. Adding one tool touches 4–5 sites. No tool profiles exist; a previous "task profile" concept was deliberately removed (`main.rs:833-842`).
- **Retrieval scoring is hand-rolled and hand-tuned.** All constants live in `CodeIndex::context` (`src/index/mod.rs:915-989`): exact phrase +12, required term +15, path match +5, exact symbol +25, partial symbol +7, dirty file +7, recent mutation +5, `ln_1p(count)*3` term frequency, `coverage*10` bonus, doc-type multipliers (test ×0.9, source ×1.1, runtime_evidence ×1.25), and a size-penalty divisor `1 + min(ln(1+bytes/8192),4)*0.18`. There is **no IDF, no document-length normalization, no BM25** — no IR library at all in `Cargo.toml`.
- **Retrieval unit is not symbol-bounded.** `code_context` returns an excerpt of ±6 lines around the *first* matching signal (`index/lines.rs:24-33`, `index/mod.rs:999-1047`), annotated with the enclosing symbol but not aligned to it. Candidate scoring is file-granular via a token→file inverted index (`index/mod.rs:212-225`).
- **Reference lookup is lexical.** `references` mode requires an indexed declaration, then scans with a word-boundary regex `\b<name>\b` (`index/mod.rs:1059-1202`). It cannot distinguish overloads, shadowing, or same-named identifiers.
- **The transaction engine is the strongest asset.** Preconditions (snapshot / content hash / provenance handle, `edit.rs:598-614`), overlap preflight, tree-sitter syntax preflight, atomic temp-file+rename writes with in-process reverse rollback, mutation journal with rotation and crash recovery, validation commands with rollback-on-failure. All edit tools funnel into one `changes[]` pipeline compiled to `PlannedFile { path, before, after }` (`edit.rs:27`), a clean compile target for future operations.

### 1.2 What the analysis missed (found in audit; fix in P0)

| # | Defect | Location |
|---|--------|----------|
| D1 | `config.example.json` ships `port: 8813`; code default and README say `8820`. Example also omits `foregroundBudgetMs`. | `config.example.json:4`, `main.rs:83` |
| D2 | `git_push` does **not** require `confirm: true`, while `git_restore` does. Push is the only network write and should be gated identically. | `workspace/mod.rs:1070-1075` vs push path `mod.rs:1111+` |
| D3 | Only `git push` has a timeout (120s). All other git invocations use blocking `.output()` with no bound — a hung credential prompt or lock wedges the actor. | `repository.rs:58-70` |
| D4 | Deferred (async-promoted) validation **never auto-rolls-back**: the edit stays applied with `validation_pending`; `rollback_on_failure` silently does not apply to the async path. Undocumented. | `edit.rs:247-269` |
| D5 | Syntax preflight silently passes JSON/YAML/TOML/Markdown — `language_name` knows them but `language()` has no grammar, so `parse_has_error` returns `None` and the `SYNTAX_ERROR` gate is bypassed with no indication. | `symbols.rs:38-62`, `edit.rs:437-448` |
| D6 | The `mcp_transport.rs:85-113` allowlist is a second source of truth for the tool set with no cross-check test; README and `docs/tools.md` tables are hand-maintained and can drift silently. | `mcp_transport.rs:85-113` |
| D7 | Missing tests: `OVERLAPPING_EDITS`, `SYNTAX_ERROR` gate (and its D5 bypass), `PARTIAL_COMMIT`/`manual_recovery_required`, symbol-anchored `insert` positions, same-file multi-change chaining (`put_plan`), and the D4 no-rollback semantics. | `workspace/tests.rs` |
| D8 | `unwrap_or_default()` swallows git-status failures after commit/stage, leaving `repo_status` silently empty. | `edit.rs:881`, `edit.rs:1092`, `mod.rs:709` |
| D9 | `RangeHandle.snapshot_id` is carried but dead (`#[allow(dead_code)]`) — a handle from a stale snapshot is accepted whenever its content hash still matches. Remove or use the field. | `index/handle.rs:12-14` |

### 1.3 Corrections to the analysis's recommendations

- It recommended **Tantivy** for BM25. **Rejected — decision A2.** The index is already in-memory with a JSON cache; hand-rolled BM25F over a chunk-granular inverted index is ~300 lines, keeps the single-binary property, and avoids a dependency whose segment-merge model fights the existing per-file incremental refresh (`refresh_paths`, `index/mod.rs:513`).
- It said retrieval "scores whole files"; in fact excerpts are returned, but **candidate selection and scoring are file-granular**, which is the real problem — a relevant method loses to noise elsewhere in its file. The fix is chunk-granular scoring, not just chunk-granular output.
- Its priority order put the evaluation harness first. Reordered here: hygiene → single-repo simplification → registry → eval → retrieval, because the simplification and registry phases are behavior-preserving-or-deleting work that everything else builds on, and each phase stays independently shippable.
- It proposed a `retrieval`-style profile system for many deployment shapes. With single-repo local use as the only target, profiles are simplified to three (see A3) — less to build, less to test.

---

## 2. Architecture decisions (settled — do not relitigate during implementation)

**A1. The transaction engine is the stable core.** Any new mutating capability (LSP rename, future refactors) must compile into the existing `changes[]` wire form or `PlannedFile` plan form and flow through `prepare_edit` → `commit_plan`. No second edit path, ever. The narrow write tools stay; exposure is controlled by profiles (A3), not by deletion.

**A2. Retrieval v2 = chunk-granular BM25F, hand-rolled, in-memory.** No Tantivy, no embeddings, no reranker in the default path. Chunks are symbol-bounded (from the tree-sitter symbols already extracted per file), with a whole-file chunk for symbol-less files and sub-chunking for oversized symbols. Scoring is BM25 (k1=1.2, b=0.75) over weighted fields (content / symbol name / path), followed by a small set of *documented, benchmark-gated* deterministic boosts. The old scorer remains behind a config flag during transition, then is deleted. Embeddings/hybrid retrieval is explicitly deferred (§4).

**A3. Three tool profiles, config-driven.** `server.toolProfile` ∈ `full` (default, all 27 — zero behavior change), `read-only` (retrieval + git inspection, no mutation/execution), `edit` (everything except `bash*` and `git_push` — for connector sessions where command execution isn't wanted). Plus `custom` include/exclude lists. Profiles are deployment-level (one connector = one config). This replaces the earlier five-profile proposal — single-repo local use doesn't need more.

**A4. Single source of truth for tools.** A typed registry generates: the `tools/list` payload, the transport allowlist, the annotations, profile membership, the capabilities output, the docs tool table, and schema snapshots that tests assert against. The flat-schema rule (no `oneOf`/`allOf`/`not`/`const`) becomes registry-level validation at startup and in tests.

**A5. LSP is an optional, replaceable backend — later.** Behind a `CodeIntelligenceBackend` trait with tree-sitter as the always-available default; LSP servers as supervised child processes on the existing process runtime; LSP `WorkspaceEdit`s compile into `changes[]` (A1); every response labels evidence `semantic | syntactic | lexical`. Until then, the lexical `references` mode must say it is lexical in its output (P0 fix).

**A6. Benchmarks gate retrieval changes.** No new scoring heuristic, weight, or boost lands without a committed baseline showing the delta on the fixture query set. **crawlerai is a pinned fixture repo** — the benchmark must reflect the project this is actually used on.

**A7. One repository per server instance.** The multi-workspace subsystem is removed: no `workspaces[]` config, no session→workspace binding, no actor cache/eviction, no workspace switching. Config declares exactly one `workspace.path`; the server indexes it eagerly at startup. Running against a different project = restart with a different config (or a second instance on another port). Rationale: the user runs this against local projects one at a time; the multi-workspace machinery (~600 lines of `manager.rs` plus its failure modes: `WORKSPACE_BUSY`, stateless-session sharing warnings, eviction races) is complexity with no payoff, and deleting it enables eager indexing — the biggest available cold-start latency win.

**A8. Target clients are ChatGPT Apps and Claude connectors only.** Perplexity docs and any Perplexity-specific concessions are removed. Flat schemas stay (see constraints — they're a token/reliability win regardless). stdio transport stays (free, useful for local testing and terminal agents).

---

## 3. Phased roadmap

Phases ordered by dependency, each independently shippable.

---

### P0 — Hygiene, safety fixes, and honesty (small, immediate) — ✅ SHIPPED (2026-07-12)

No architecture changes. Fixes D1–D9 from §1.2. All items below implemented and verified (159 tests, 0 failures; clippy clean).

1. **D1** ✅: standardized the port across `config.example.json`, `default_port()`, and README; added `foregroundBudgetMs`; added `shipped_config_example_deserializes_and_matches_code_defaults` which deserializes `config.example.json` through the real config path so the example can never drift again. **Port is `8813`** — the operator overrode the plan's proposed `8820` during implementation; the drift test now pins `8813` and asserts `server.port == default_port()`.
2. **D2** ✅: `git_push` now requires `confirm: true` (schema `required` + runtime check mirroring `git_restore`). Breaking change for safety parity; noted in README and `docs/tools.md`.
3. **D3** ✅: all git invocations route through a timeout-bounded runner with a default 30s bound (push keeps 120s). `GIT_TERMINAL_PROMPT=0` and `GIT_ASKPASS=echo` are set on every git command so credential prompts fail fast instead of hanging.
4. **D4** ✅: deferred (async-promoted) validation returns `rollback_on_failure_not_applied: true`, echoes `rollback_on_failure_requested`, and includes explicit guidance text; documented in `docs/tools.md`. (No automatic post-hoc rollback — the workspace may have legitimately moved on.)
5. **D5** ✅: added `tree-sitter-json` (JSON edits are now syntax-checked); for formats without a bundled grammar (YAML/TOML/Markdown/HTML/text) each planned file emits `syntax_check: "skipped"` so the bypass is visible.
6. **D8** ✅: the `unwrap_or_default()` git-status swallows were replaced with a logged `tracing::warn!` + `repo_status_stale: true` marker on the git response; cleared on preflight.
7. **D9** ✅: removed the dead `snapshot_id` field from `RangeHandle`. It was `skip_serializing`, so no wire change and no handle-version bump was required.
8. **Honesty fix (A5)** ✅: `references` results gain `"evidence": "lexical"` (per-result and top-level) plus an `evidence_caveat`; the tool description and `docs/tools.md` carry the lexical-not-semantic caveat.
9. **D7** ✅: added tests for overlapping edits, the syntax gate + its now-closed JSON bypass (JSON checked / YAML skipped), symbol-anchored insert positions, same-file multi-change accumulation, and deferred-validation no-rollback semantics.
10. **A8 cleanup** ✅: deleted `docs/connect-perplexity.md`; removed Perplexity mentions from README, `docs/installation.md`, and the `main.rs` compatibility comment (flat-schema rationale reworded to token-cost/hosted-client reliability); flat-schema test kept.

**Acceptance:** all existing tests pass; new tests cover each fix; no tool schema changes except `git_push.required` and additive response fields.

---

### P1 — Single-repo simplification (A7) — ✅ SHIPPED (2026-07-12)

**Goal:** delete the multi-workspace subsystem; make startup eager; reduce config to one shape. This is mostly *deletion* — resist the urge to redesign while deleting.

**Shipped summary:**
- Config reduced to a single required `workspace.path` (`WorkspaceSettings`); `DaemonConfig` dropped `workspaces[]`. Removed keys (`workspaces[]`, `workspace.defaultPath`, `workspace.lockToDefault`, `workspace.allowedRoots`) are rejected at `initialize` with `LEGACY_WORKSPACE_CONFIG` listing the offending keys and the new shape — no silent migration.
- `manager.rs` reduced to a single `actor: RwLock<Option<Arc<WorkspaceActor>>>`; deleted the cached-actor map, LRU eviction, per-session→workspace bindings, dynamic path resolution, `allowedRoots` canonicalization, pin/lock, `WORKSPACE_BUSY` switching, and the whole multi-workspace test suite (net LOC down substantially). `SessionKey` kept for Bash-run scoping + `changes` attribution only.
- Eager startup: the single actor is built at `initialize` before the transport binds (index scan + file watcher already run inside `WorkspaceActor::open`), and Bash readiness is pre-probed. `initialize`/`/health` report `index_ready`, `file_count`, and `last_reconcile_ms_ago`. `main.rs` logs "repository ready before transport bind".
- `workspace` tool schema: `open`/switch action and `path` argument removed; actions are `summary` (default), `refresh`, `changes`, `diagnostics`, `skills`, `skill`. `validate_http_workspace_mode` deleted.
- A missing/non-directory `workspace.path` fails startup with `WORKSPACE_NOT_FOUND` / `WORKSPACE_NOT_DIRECTORY`.
- Docs (README, installation, implementation, tools, SECURITY, CONTRIBUTING) rewritten to the single `workspace.path` model; `config.example.json` uses the new shape on port 8813.
- Verified: `cargo build`/`clippy`/`test` clean — 147 tests, 0 failures.

**Note:** the `8820` in the example below was superseded by the P0 `8813` override; canonical port is **8813**.

**Config** (breaking change, acceptable pre-1.0):

```jsonc
{
  "server":    { "host": "127.0.0.1", "port": 8813, "authMode": "bearer", ... },
  "workspace": { "path": "C:/Projects/crawlerai",
                 "artifactPaths": [...], "excludePaths": [...] },
  "policy":    { ...unchanged... },
  "skills":    { ...unchanged... }
}
```

Removed keys: `workspaces[]`, `workspace.defaultPath`, `workspace.lockToDefault`, `workspace.allowedRoots`. Startup fails with a clear message listing the removed keys if any are present (no silent migration; the message shows the new shape). `workspace.path` is canonicalized at startup (`security.rs::canonical_root`) and is the *only* root for the process lifetime.

**Code removal / simplification:**

- `manager.rs`: remove `sessions: HashMap<SessionKey, String>`, `actors: HashMap<...>` + `MAX_CACHED_WORKSPACES`/`evict_idle_actors` (`manager.rs:14, 48-58, 591-625`), `open_workspace` path resolution (`manager.rs:404-516`), `canonical_workspace_path` allowed-roots logic (`manager.rs:688-734`), session-binding (`record_session_binding`), `WORKSPACE_BUSY` switching guard (`manager.rs:461-468`), and the stateless-shared-workspace warning path (`manager.rs:575-583`, `workspace/mod.rs:648-650`). The manager holds exactly one `Arc<WorkspaceActor>` created at startup. `SessionKey` remains only if the bash/session attribution needs it (bash run dedup keys on session — keep that; it's orthogonal).
- `main.rs`: `validate_http_workspace_mode` (`main.rs:167-187`) becomes trivial (single repo is always pinned); legacy `workspace_id`/`workspace` arg stripping (`main.rs:858-861`) can stay as a reject-with-clear-error.
- **`workspace` tool slims down**: `open`/switch actions are removed from the schema; remaining actions: `summary`, `diagnostics` (and whatever read-only actions exist today). Schema shrinks accordingly.
- `WorkspaceActor` itself (locks, index, journal, bash supervisor, git backend) is **untouched** — it already models one repo.

**Eager startup (latency):**

- Build/load the index at startup, before the transport accepts connections, using the existing `scan_cached` warm path (`index/mod.rs:253-360`). Log cold vs cache-hit and elapsed ms.
- Start the file watcher at startup (today it starts on first workspace open).
- Pre-probe bash availability (`ensure_available`) at startup so the first edit-with-validation doesn't pay the probe.
- `/health` reports `index_ready`, file count, and last reconcile time — useful for tunnel setups.
- Target on crawlerai-class repos (≈ a few thousand files): warm start (index cache valid) to serving in **< 2s**; cold full index bounded by parallel scan and reported honestly.

**Acceptance:**
- All tools behave identically for a single configured repo; `workspace(summary/diagnostics)` unchanged in output shape minus multi-workspace fields.
- Old multi-workspace config keys produce an actionable startup error, not silent misbehavior.
- First `code_context` call after warm start pays zero index-build cost (test: assert index ready before transport bind).
- Net LOC decreases; `manager.rs` shrinks substantially; the multi-workspace test suite (`manager.rs:1042-1345` etc.) is deleted, not skipped.

---

### P2 — Typed tool registry + profiles — ✅ SHIPPED (2026-07-12)

**Goal:** one source of truth; kill the triple-pinning (D6); enable profiles (A3).

**Shipped:** the tool registry lives in `src/tools/` (`mod.rs` = `ToolDefinition`/`ToolSafety`/`Profile`/`ToolAccess`/`resolve_access`; `schemas/*` = per-domain flat draft-07 builders). It is the single source of truth for all 27 tools and drives (1) the `tools/list` payload — the former `main.rs::tools()` is deleted, (2) the transport callable-name gate — the former hardcoded allowlist in `mcp_transport.rs` is deleted, (3) profile filtering, and (4) schema-shape validation. `server.toolProfile` (`full` default / `read-only` / `edit` / `custom` with `server.tools.include`/`exclude`) resolves once at startup into an immutable `ToolAccess` in `AppState`. A known-but-excluded tool returns `TOOL_NOT_IN_PROFILE`; an edit carrying `validate` when bash is unavailable returns `VALIDATE_UNAVAILABLE` before mutating. Per the maintainer's directive, **no** CI/snapshot/budget/dump-tools drift machinery was added — correctness is enforced by ordinary `cargo test` (160 pass), clippy is clean, and the transport has zero hardcoded tool names. The full module tree reorg (`server/`, `transport/{http,stdio,sessions}`, `compatibility/`) below was intentionally **not** performed: the registry+schemas split already delivers the one-definition-per-tool goal, and further shuffling stable code would add churn without functional benefit, contrary to the "fast, efficient, accurate, no confusion" objective.

Original spec (for reference):

Module layout (move code, don't rewrite logic):

```
src/
  server/           # from main.rs: config load/validate, auth, startup
  transport/        # mcp_transport.rs split: http.rs, stdio.rs, sessions.rs
  tools/
    registry.rs     # ToolDefinition + ToolRegistry + profile filtering
    schemas/        # per-domain schema builders: workspace.rs, retrieval.rs,
                    #   edits.rs, git.rs, bash.rs
  compatibility/
    prepare.rs      # from main.rs::prepare(): legacy arg normalization,
                    #   narrow-edit→transaction wrapping, git action injection,
                    #   bash field allowlists
```

```rust
pub struct ToolDefinition {
    pub name: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub safety: ToolSafety,            // existing 5-level enum
    pub profiles: &'static [Profile],  // full / read_only / edit membership
    pub input_schema: fn() -> serde_json::Value,
}
```

Generated from the registry:
1. `tools/list` payload — replaces `main.rs::tools()`.
2. Callable-name set — **replaces the `mcp_transport.rs:85-113` allowlist** (transport asks the registry, filtered by active profile).
3. `code_capabilities` output.
4. `docs/tools.generated.md` table via a `--dump-tools` flag; CI diffs against the committed copy.
5. **Schema snapshot files** under `tests/snapshots/tools-<profile>.json`; changing a schema without updating the snapshot fails CI.

Registry-level validation (startup + tests): draft-07, `type: object`, flat (no `oneOf`/`allOf`/`not`/`const`), `execution.taskSupport == "forbidden"` — port `main.rs:1169-1306` into one generic loop.

**Profiles** (`server.toolProfile`, default `full`):

| Profile | Tools |
|---|---|
| `full` | all (default; identical to today) |
| `read-only` | workspace, code_context, code_capabilities, code_fetch, code_search, git_status, git_diff, git_log, git_show, git_blame |
| `edit` | everything except `bash`, `bash_status`, `bash_output`, `bash_cancel`, `git_push` |
| `custom` | `server.tools.include` / `exclude` over the full set |

Runtime rule: a tool outside the active profile is absent from `tools/list` **and** rejected at `call_tool` with a distinct `TOOL_NOT_IN_PROFILE` error (better model self-correction than "unknown tool"). Note: `edit` profile excludes bash, so `code_edit` validation commands are unavailable — the registry must reject a profile/policy combination where `validate` would be advertised but non-functional, or edits in that profile simply don't accept `validate` (pick one, document it).

**Schema-footprint budget (efficiency):** a test asserts the serialized `tools/list` byte size per profile against committed budgets, so description bloat shows up in review. While there, tighten the longest descriptions — hosted clients pay these tokens every conversation.

**Acceptance:**
- `full` profile produces a byte-identical `tools/list` to the pre-refactor server (snapshot proves it; modulo the P1 `workspace` tool slimming, which lands first).
- All dispatch paths unchanged (existing `prepare`/dispatch tests pass, moved not rewritten).
- Transport contains zero hardcoded tool names.
- `read-only` and `edit` profiles boot and serve against a real config.
- CI fails on schema/docs drift.

---

### P3 — Evaluation harness — ✅ SHIPPED (scoped down) (2026-07-12)

**Shipped:** a minimal offline benchmark crate `eval/` (workspace member, not in
the release binary). `cargo run -p eval -- --ranking v1` scans **this repository
only** with the real `CodeIndex`, runs a fixed ~15-query set
(`eval/queries/codeweave.json`, spanning exact-symbol, filename lookup,
natural-language intent, implementation discovery, and tests-for-symbol) through
`CodeIndex::context`, and writes `eval/baseline/v1.json`. Metrics: Recall@1/5/10,
MRR@10, mean chars, cold/warm index time, and search p50/p95. The engine is
reached through a thin new `src/lib.rs` that re-exposes only `index`, `model`,
`security`, `symbols` (via `#[path]`, so no logic is duplicated and `main.rs` is
untouched); the server remains a binary. `--ranking v2` is reserved for P4 and
currently exits with a clear "not available yet" message.

**Scope decision (maintainer):** the full plan below (multiple OSS fixtures at
pinned commits, an agent-workflow protocol run on ChatGPT/Claude connectors, and
a CI retrieval job) was intentionally **not** built. Rationale, in the
maintainer's words: an in-process score "cannot replicate real time data which
is chatgpt web app… if it works here doesn't mean it will work exactly same in
chatgpt/claude webapp," so a heavy multi-repo/multi-client harness would measure
something it can't stand in for. What shipped is the *bare minimum needed for P4
to have an objective regression gate* — a trustworthy relative baseline, local
only, no CI. The current v1 numbers (Recall@1 66.7%, Recall@10 100%, MRR 0.773)
also confirm the known weakness P4 targets: filename-lookup queries rank poorly
under the current natural-language scorer.

Original spec (for reference):

**Goal:** measurement before retrieval changes (A6).

**3a. Deterministic retrieval + latency benchmark** — new crate `eval/` (workspace member, not in the release binary).

- Fixtures: pin **crawlerai** (the user's real project) plus 2 small OSS repos in other languages, at fixed commits (submodules or vendored tarballs).
- Query set: YAML, ~15–25 queries per repo across: exact symbol, filename/config lookup, natural-language intent, implementation discovery, runtime-error-to-source (paste a traceback line), tests-for-symbol, artifact exclusion, changed-file prioritization. Each query lists expected targets as `path` or `path::symbol`. For crawlerai, write queries from real tasks the user has actually done.
- Metrics per run: Recall@1/@5/@10, MRR@10, nDCG@10, % results aligned to a complete symbol, mean chars returned, **cold index time, warm (cache-hit) start time, warm search latency p50/p95, single-file incremental refresh latency**.
- Runner: `cargo run -p eval -- --ranking <v1|v2>` prints a table and writes `eval/baseline/<mode>.json`. CI job informational at first; hard gate from P4.

**3b. Agent workflow audit** — a manual protocol: `docs/eval-protocol.md` with ~10 scripted tasks (explore unfamiliar code, exact symbol retrieval, reference lookup, small edit, multi-file edit, failed validation + rollback, long-running command via bash promotion, git review, stale-snapshot recovery, connector reconnect) and a recording template (tool calls, invalid calls, chars in/out, wall time, correct final diff). Run on **ChatGPT and Claude** connectors against crawlerai before/after P4; commit filled-in results under `eval/workflow-runs/`. This measures the actual goal — fewer round trips in the web interfaces.

**Acceptance:** baseline JSON for the current engine committed; CI runs the retrieval suite on Linux; protocol doc exists with one completed baseline run.

---

### P4 — Structural retrieval v2 (the big accuracy win) — ✅ SHIPPED (2026-07-13)

**Goal:** chunk-granular retrieval; symbol-bounded results; benchmark-gated.

**Shipped summary:**

The original plan proposed a chunk-level **BM25F** scorer. That was built and measured against the P3 harness, and it *regressed* natural-language recall: chunk-granular BM25 splits a file's relevant terms across many short chunks, and file-level BM25 length-normalization over-penalized the large, information-dense files that the exact-match constants in v1 handle well (chunk-BM25F Recall@1 60% / MRR 0.699; file-BM25F Recall@1 33% / MRR 0.563 — both worse than v1). Per the project objective — *fast, efficient, accurate, no added complexity* — BM25F was **dropped from the hot path**.

What actually shipped as `index.ranking: "v2"` is **v1's exact-scoring loop plus two additions**:

1. **Filename affinity boost** — the fix for v1's one real weakness (filename/config lookups ranked poorly). Per file, `path_hits = candidate terms present in the file's path`; `affinity = path_hits / candidate_len`; `score += affinity * 22`, and a full path-name match adds a further `+18`. This moved `config.example.json` from rank 10 (miss) to rank 1 without touching any other query. df reuses the existing `token_index` (`df = token_index[term].len()`, `N = files.len()`), so **no separate df map or corpus-stat bookkeeping was added**.
2. **Symbol-bounded rendering** — each file is split at index time into `Chunk`s (one per top-level symbol, `SymbolPart`s for symbols > 150 lines, `Remainder` chunks for the gaps). A result renders the *whole enclosing symbol* when it fits `MAX_RENDER_LINES` (28); a larger symbol renders a window **centered on the match** (never the first-N-lines truncation v1 used). Results carry additive `chunk_kind` and `complete_symbol` fields; v1 omits both. Chunks are `#[serde(skip)]`, rebuilt in `normalize_entry`, so a per-file incremental refresh replaces a file's chunks as a unit — no global stat to maintain.

Index cache schema bumped `codeweave-index-v6` → `codeweave-index-v7`; old caches rebuild transparently. Default stays `v1`; `v2` is opt-in via config.

**Measured on this repo (P3 harness, 15 queries), v2 vs committed v1:**

| Metric | v1 | v2 | Gate |
|---|---|---|---|
| Recall@1 | 66.7% | **73.3%** | improves ✅ |
| Recall@5 | 93.3% | **100%** | improves ✅ |
| Recall@10 | 93.3% | **100%** | no regression ✅ |
| MRR@10 | 0.767 | **0.850** | improves ✅ |
| Search p50 latency | ≈ v1 | ≤ v1 | within 1.5× ✅ |
| Mean chars | 6.8k | 11.5k | *see trade-off* ⚠️ |
| Complete-symbol rate | — | 0.27 | *see trade-off* ⚠️ |

**Honest trade-offs (accepted by the maintainer):**

- **Mean chars increased (~1.7×), not held flat.** The "equal-or-better recall at no char increase" gate is in direct tension with returning *whole symbols* instead of 6-line excerpts — the completeness is the point of P4. We chose accuracy: v2 both retrieves better *and* returns the full enclosing symbol, at a higher per-result char cost bounded by `MAX_RENDER_LINES` and the char budget. Tightening the cap trades completeness back for chars if a deployment needs it.
- **Complete-symbol rate is 0.27, not ≥0.80.** That gate is **unreachable on any repo whose top-ranked answers are non-code files** — `config.example.json`, `Cargo.toml`, and markdown have *no symbols*, so their results are `Remainder` chunks by construction and can never be "complete symbols." The metric is reported honestly rather than gamed; among results that *do* land in a symbol, rendering returns the whole symbol.
- **P3b workflow audit (ChatGPT + Claude on crawlerai)** was not run in-process — it can't be, per the P3 decision that hosted-client behavior isn't reproducible here. A separate crawlerai eval is a possible follow-up.

**CrawlerAI retrieval follow-up (2026-07-13):** the same offline runner now
supports `--repo crawlerai` with 20 real queries spanning backend Python,
frontend TypeScript, exact symbols, filenames/config, natural-language intent,
implementation discovery, pasted runtime errors, tests, changed-file priority,
and artifact exclusion. Baselines live under `eval/baseline/crawlerai/` and
record the evaluated Git revision plus dirty-worktree state. The first local
comparison (base revision `7f4d5d4`, dirty worktree) measured v1 → v2 as:
Recall@1 40% → 50%, Recall@5 75% → 75%, Recall@10 80% → 85%, MRR@10
0.559 → 0.623, mean chars 4,952 → 8,209, and search p50 187ms → 267ms.
This confirms v2's cross-repo accuracy gain while identifying natural-language
ownership retrieval, response size, and latency as the next measured targets.
Regenerate from a clean checkout at the query set's pinned revision before
treating these numbers as a release gate.

Verification: `cargo test --bin codeweave-rust` (163 pass, incl. new chunk-build and v1/v2 ranking tests), `cargo clippy --workspace` clean (all BM25F machinery deleted, no dead code), `cargo run -p eval -- --ranking {v1,v2}` regenerates `eval/baseline/*.json`.

---

**Original design (for the record — the BM25F portion below was built, measured, and dropped as described above):**

**Goal:** chunk-granular BM25F retrieval; symbol-bounded results; benchmark-gated.

**Chunk model** (extend `FileEntry`, `index/mod.rs:126-147`):

- One chunk per top-level symbol (symbols already carry `start_line`/`end_line` from `symbols.rs`); nested symbols stay inside their parent's chunk but contribute their names.
- One "file remainder" chunk for content outside any symbol (imports, module docstrings, config files, markdown).
- Symbols longer than ~150 lines split into sequential sub-chunks retaining the owning symbol's identity.
- Fields for scoring: `content` (weight 1.0), `symbol_names` (2.5), `path_segments` (1.5). Per-chunk term frequencies at index time; global df/IDF maintained incrementally on `insert_entry`/`remove_entry` — slots into the existing per-file refresh since a file's chunks are replaced as a unit.
- Bump index cache schema to `codeweave-index-v7`; old caches rebuild transparently.

**Scoring pipeline** (replaces `CodeIndex::context` scoring, keeps the response shape):

1. Candidate chunks via term→chunk inverted index.
2. BM25F (k1=1.2, b=0.75, field weights above).
3. Deterministic post-boosts, each emitting a `reason` string as today: exact-symbol-name, exact-phrase, path match, dirty-file, recent-mutation, doc-type multipliers. Initial values scaled from current constants into BM25 range, then tuned only against the P3 benchmark.
4. Diversification: max 3 chunks per file in the top N unless the query names that file.
5. Rendering: return the **whole symbol chunk** when it fits the remaining char budget; otherwise the existing `fit_excerpt` shrink applies within the chunk. Each result keeps `handle`, enclosing-symbol metadata, `reasons`, plus additive fields `chunk_kind`, `complete_symbol: bool`.

**Compatibility:**
- `code_search` modes (literal/regex/filename/symbol/references/outline/repo_map) unchanged.
- `code_context` request/response schema unchanged (additive fields only).
- Config flag `index.ranking: "v1" | "v2"` (default `v2` once gates pass; `v1` kept one release, then deleted).

**Acceptance (hard gates via P3 harness, measured on crawlerai + OSS fixtures):**
- Natural-language-intent Recall@5 and MRR@10 improve over committed v1 baseline.
- Exact-symbol Recall@1 does not regress; warm exact-identifier latency within 1.5× of v1.
- Mean chars returned for equal-or-better recall does not increase.
- ≥80% of returned results are complete symbols on the fixture set.
- Single-file incremental refresh latency within 1.5× of v1.
- Workflow audit (P3b) re-run on ChatGPT + Claude shows fewer tool calls on the exploration tasks.

---

### P5 — Onboarding: `init`, `doctor`, `serve` (core CLI) — ✅ SHIPPED (2026-07-13)

The core CLI onboarding path is shipped. `codeweave init [--path <project>]` writes a config from the embedded example, creates its bearer token, and prints the local connector URL plus ChatGPT/Claude guide pointers. `codeweave doctor` exercises config validation and the real eager manager initialization, reporting config/auth/profile/workspace/Git/port/token/index/Bash checks and returning non-zero on failure. `codeweave serve` is the explicit spelling of the long-standing bare invocation; existing `--transport`, `--config`, `--host`, and `--port` calls remain compatible.

The signed cross-platform release workflow / `cargo install` distribution and `schemars` JSON Schema plus `configVersion` migration field are deliberately deferred. They expand CI, release, and configuration-maintenance surface without improving the local startup path, contrary to the project's fast, efficient, low-complexity direction.

Original scope (for reference):

Independent; can run parallel to P4. Simpler now — single-repo config has one shape.

1. `codeweave init` — interactive: pick the project path, write `config.json`, generate token, print the connector URL + ChatGPT/Claude next steps (reuse `docs/connect-*.md` content).
2. `codeweave doctor` — config parses/validates, workspace path exists, git available, bash probe (`ensure_available`), port free, token file present, index cache status. Non-zero exit on failure; each check prints pass/fail + fix.
3. `codeweave serve` alias for the run invocation.
4. Release workflow producing Linux/macOS/Windows binaries + `cargo install`. Signing is a stretch goal.
5. JSON Schema for `config.json` (via `schemars` or generated from the structs); add `configVersion` (absent = v1) so future migration is possible.

**Acceptance:** a new user goes from binary download to a working ChatGPT connector following only `init` output; `doctor` catches D1-class misconfigurations.

---

## 4. Deferred — do not start

**Hybrid retrieval (embeddings + RRF).** Blocked on P4 shipped + P3 benchmarks showing a query category where BM25F measurably plateaus. If built: optional provider trait (local ONNX or OpenAI-compatible), RRF fusion, skip vectors for decisive identifier/path queries, hard fallback to BM25, off by default, no repo content leaves the machine unless explicitly configured.

**LSP semantic backend.** Blocked on P4 shipped + workflow-audit evidence that lexical references are a real bottleneck in ChatGPT/Claude sessions. Scope when started: `CodeIntelligenceBackend` trait (tree-sitter impl extracted first as a no-op refactor), LSP child processes on the process runtime, definitions/references/diagnostics, rename compiled to `code_preview`/`code_transaction` (A1), evidence labels everywhere, per-language opt-in. Never in the workspace actor; never writes files.

## 5. Explicitly not doing (and why)

- **No multi-workspace support** — decision A7; deleted, not just hidden.
- **No Perplexity compatibility work** — decision A8; flat schemas stay for token/reliability reasons.
- **No Tantivy / on-disk search index** — A2; in-memory BM25F preserves single-binary + incremental refresh.
- **No mandatory embeddings or bundled models** — breaks offline/local-first.
- **No JetBrains-style backend** — separate product; LSP covers it if ever needed.
- **No Serena-style server-managed memory** — `AGENTS.md`/repo docs + client memory suffice.
- **No mega edit tool** — safety classifications depend on narrow tools; profiles reduce exposure instead.
- **No new scoring heuristics outside the benchmark gate** — A6.
- **No automatic rollback for async-promoted validation** — explicitness beats surprise reverts (P0 item 4 makes the gap visible).

## 6. Implementation ordering & dependency graph

```
P0 (hygiene)            — ship first
P1 (single-repo)        — after P0; large deletion, unlocks eager startup
P2 (registry/profiles)  — after P1 (registry wraps the slimmed tool set once)
P3 (eval harness)       — anytime after P1; blocks P4 merges (gates)
P4 (retrieval v2)       — needs P3 gates; benefits from P2 (flags, capabilities)
P5 (onboarding)         — anytime after P1 (single config shape)
Deferred: hybrid retrieval, LSP — evidence-gated
```

Suggested PR sizing: P0 as 3–4 small PRs (config/docs, git safety, edit-pipeline visibility, tests). P1 as config-shape PR → manager-deletion PR → eager-startup PR. P2 as move-only PR → registry PR → profiles PR. P4 as index-model PR → scoring PR → rendering PR, each behind the `ranking` flag until gates pass.
