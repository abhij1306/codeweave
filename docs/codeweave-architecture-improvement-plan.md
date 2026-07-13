# CodeWeave Architecture Improvement Plan — Revised After Tool-Surface Audit

**Status:** Proposed design freeze v2  
**Repository reviewed at:** `5658c65b0b1e913b688edeef7d1f6d6e2bf6c62f`  
**Target scale:** normally around **100k LOC**, with **300k LOC as the practical ceiling**  
**Primary metrics:** **accuracy first, latency second**  
**Public-contract rule:** do not reintroduce a generic retrieval `query` parameter  
**Implementation rule:** fix the evaluation boundary before changing retrieval behavior

---

## 1. Executive decision

CodeWeave should remain a small, local, single-repository service for ChatGPT and Claude connectors. It should not become a compiler, graph database, distributed search engine, or embedded coding agent.

The revised architecture is:

```text
                           Public MCP tools
                                  |
              +-------------------+-------------------+
              |                                       |
      explicit code_retrieve                    code_intelligence
       discriminated operations                position-based requests
              |                                       |
              +-------------------+-------------------+
                                  |
                         RetrievalFacade
                                  |
           +----------------------+----------------------+
           |                      |                      |
       TextIndex              SymbolIndex          ReferenceService
   literal/regex/file       declarations and       semantic LSP first
       discovery               outlines            when healthy
                                                        |
                                           exact full-scope fallback
                                           + Tree-sitter classification
```

The audit changes the implementation order materially:

1. **The existing evaluator does not exercise the live MCP retrieval path.**
2. The dead/eval-only ranker must not guide production architecture.
3. Public tool and operation contracts need simplification before adding more retrieval machinery.
4. The reference fix remains important, but it must be implemented against a corrected live-path evaluation harness.
5. LSP, lexical fallback, and Tree-sitter should converge through one service and one result model.
6. Complexity unrelated to measured accuracy or latency should be removed, not reorganized into more abstractions.

---

## 2. What the audit changed

## 2.1 Highest-priority finding: the evaluator benchmarks a non-production path

The current live retrieval route is:

```text
code_retrieve
  -> WorkspaceActor::code_retrieve_for_session
  -> execute_retrieval_operation
  -> search_index
  -> CodeIndex::search
```

The current evaluation route is:

```text
eval
  -> CodeIndex::context
  -> Ranking::V1 / Ranking::V2
```

`CodeIndex::context`, `src/index/context.rs`, `src/index/chunks.rs`, `FileEntry::chunks`, and `FileEntry::path_tf` are not used by any live MCP tool. They are used only by the evaluator and unit tests.

Independent verification against the current repository and the parent of commit `7410132` shows this was already true before the recent module split: the context ranker was called only by `eval` and tests.

### Decision

The current retrieval baselines are **not valid production-regression gates**. They measure an internal research ranker rather than the behavior an agent receives from `code_retrieve`.

Before any ranking, indexing, reference, or tool-surface optimization:

- rebuild the evaluator around the production retrieval boundary;
- establish live-path baselines;
- then remove the eval-only ranker and its per-file data unless a separately approved public feature needs it.

Do not wire `context()` into the public tool merely to preserve the existing evaluator. That would introduce a new natural-language retrieval path contrary to the frozen API direction.

## 2.2 Tool-surface complexity is a first-class workstream

The registry is well designed as one source of truth, but the advertised surface is still large:

- 26 tools;
- 8 edit-facing tools over one internal change engine;
- 7 `code_retrieve` operation kinds sharing one 20-property item schema;
- 6 transaction change kinds sharing one 12-property change schema;
- overlapping capability/diagnostic responses;
- a deprecated ignored parameter still advertised on every write tool.

The revised plan explicitly measures and reduces:

- invalid tool calls;
- invalid operation fields;
- retries caused by schema ambiguity;
- visible tool-schema token count;
- tool calls per coding task;
- cases where the agent switches from a narrow tool to `code_transaction`.

## 2.3 Public transaction language is currently too strong

`code_transaction` provides:

- planning;
- preconditions;
- per-file temp-write + rename;
- sequential multi-file application;
- compensating restoration after failure;
- a `PARTIAL_COMMIT` outcome when restoration itself fails.

This is not filesystem-level atomicity across multiple files.

### Decision

Keep the public name for compatibility, but freeze the truthful contract:

```text
atomic per file: yes
atomic across files: no
preflight before writes: yes
best-effort compensation after partial failure: yes
manual recovery can be required: yes
```

Do not describe the engine as fully atomic. Any future implementation may reduce the partial-commit window, but it cannot promise cross-file atomicity on a general filesystem.

## 2.4 Validation semantics need clearer naming, not rollback

Validation is intentionally post-apply and non-destructive:

```text
apply edit
-> run validation
-> report pass/fail/pending
-> preserve edit
```

This behavior is correct. The confusion comes from the generic name `validate`, historical rollback language, and the still-advertised `rollback_on_failure` field.

### Decision

- preserve non-destructive validation;
- remove `rollback_on_failure` from advertised public schemas;
- temporarily accept and ignore the legacy field in the compatibility-normalization layer;
- describe `validate` everywhere as **post-apply validation commands**;
- return explicit `validation_status: passed | failed | pending | unavailable`;
- reserve reverse recovery for internal partial-commit/bookkeeping failures.

---

## 3. Frozen product scope

## 3.1 Repository scale

Optimize for:

- common case: approximately 25k–100k source LOC;
- medium case: approximately 100k–200k LOC;
- practical ceiling: approximately 300k LOC;
- one local repository per server process;
- source files already held in a bounded in-memory index;
- incremental refresh after file changes.

The server should report:

- indexed source LOC;
- indexed file count;
- ignored file count;
- indexed content bytes;
- index memory estimate;
- cache hit/miss;
- cold and warm index duration.

The 300k figure is an evaluation and support target, not initially a hard rejection threshold.

## 3.2 Success metrics

### Accuracy

- reference precision, recall, and false-zero rate;
- correct declaration disambiguation;
- evidence honesty;
- stale-answer rate after edits;
- exact symbol and text-search correctness;
- invalid-call recovery quality;
- correct transaction and validation status.

### Latency

- cold index p50/p95;
- warm startup p50/p95;
- warm operation p50/p95 by operation kind;
- fallback reference p50/p95;
- semantic reference p50/p95;
- single-file refresh p50/p95;
- edit-to-fresh-result p50/p95;
- response serialization time.

### Agent-efficiency guardrails

- tool calls per task;
- invalid calls per task;
- schema tokens exposed;
- response characters;
- retries caused by unclear contracts;
- correct final diff rate.

## 3.3 Non-goals

CodeWeave will not:

- implement Rust, Python, or TypeScript semantic resolution itself;
- clone rust-analyzer HIR/Salsa;
- add SQLite/FTS, a graph database, SCIP protobuf, Kythe serving tables, or Stack Graphs without measured need;
- add mandatory embeddings or an internal agent;
- add a generic public retrieval `query`;
- claim same-name lexical occurrences are semantic references;
- promise cross-file atomic filesystem transactions;
- preserve dead research code in the production index merely because an old evaluator uses it.

---

## 4. Public tool-surface design

## 4.1 Preserve simple tools; do not collapse everything into one mega-tool

The audit correctly notes that the narrow edit tools are adapters over one engine. That does not make them useless. They provide:

- smaller schemas;
- clearer intent;
- better safety annotations;
- simpler telemetry;
- fewer malformed one-item transaction payloads.

### Decision

Keep these primary simple tools:

- `code_write`
- `code_replace`
- `code_replace_range`
- `code_insert`
- `code_delete`
- `code_rename`

Keep `code_preview` and `code_transaction` for coordinated multi-file changes.

Do **not** replace the narrow tools with only `code_transaction`.

## 4.2 Reduce visible complexity through profiles and contract cleanup

Do not immediately delete useful tools. First measure the tool surface.

Add an evaluated **coding** profile, while keeping compatibility profiles:

```text
coding:
  workspace
  code_retrieve
  code_intelligence
  narrow edit tools
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

Keep in `full` only:

- Git staging/commit/restore/push and less-common Git inspection;
- capability/admin tools that evaluations show are not needed on every task.

The exact profile membership is frozen only after the workflow evaluation. Do not change the default profile in the same release that introduces the profile.

## 4.3 No generic `query`

`code_retrieve` remains an explicit batch of discriminated operations:

```json
{
  "operations": [
    {"operation": "find_symbol", "symbol": "ReferenceService"},
    {"operation": "search_text", "pattern": "validation_failed", "syntax": "literal"},
    {"operation": "read", "target": "path", "value": "src/references/mod.rs"}
  ]
}
```

The server does not contain an agent or semantic query planner. A generic prose `query` would therefore be only another lexical/heuristic bag and would blur the contract.

## 4.4 Replace prose-only operation rules with one typed contract table

Flat schemas are retained because hosted clients have historically handled conditional schemas unreliably. The solution is not `oneOf`; it is a single internal contract model.

Introduce:

```rust
struct OperationContract {
    operation: RetrievalOperation,
    required_fields: &'static [&'static str],
    optional_fields: &'static [&'static str],
    defaults: &'static [FieldDefault],
}

struct ChangeContract {
    kind: ChangeKind,
    required_fields: &'static [&'static str],
    optional_fields: &'static [&'static str],
}
```

Generate from these contracts:

- runtime validation;
- `code_capabilities`;
- schema field descriptions;
- documentation tables;
- contract tests;
- actionable `INVALID_OPERATION_FIELDS` / `INVALID_CHANGE_FIELDS` errors.

Behavior:

- missing required fields are listed;
- fields invalid for the selected operation are listed;
- a corrected example call is returned in `suggested_calls`;
- no field is silently ignored except explicitly versioned compatibility inputs.

This removes the second-source-of-truth problem without nested schema logic.

## 4.5 Remove the deprecated rollback field from advertised schemas

Migration:

1. stop advertising `rollback_on_failure`;
2. keep accepting it in request preparation for one compatibility release;
3. attach a deprecation warning when received;
4. remove compatibility acceptance after connector schema and workflow tests show no use.

This must apply to every narrow edit wrapper and `code_transaction` from one shared compatibility function.

## 4.6 Clarify handle semantics

Freeze the distinction:

- `code_replace(handle=...)`: exact-text replacement constrained to the fetched range;
- `code_replace_range`: replace the entire fetched line range.

Improve titles and descriptions to use the phrases:

- **replace text within fetched range**
- **replace complete fetched range**

Add paired examples and contract tests. Do not merge these operations until agent evaluations show the distinction remains confusing after improved wording.

---

## 5. Retrieval and reference architecture

## 5.1 Purpose-specific components

```text
TextIndex
  exact literal, regex, filename and discovery operations

SymbolIndex
  declarations, exact symbol selectors and outlines

ReferenceService
  target resolution and reference retrieval

LspManager
  semantic definitions, references, diagnostics and rename

FallbackOccurrenceScanner
  complete exact-name scope scan with Tree-sitter classification
```

A ranked search index may accelerate discovery. It must not determine semantic-reference completeness.

## 5.2 Reference target forms

No public backend selector is added.

Internal target model:

```rust
enum ReferenceTarget {
    SymbolSelector {
        symbol: String,
        definition_path: Option<String>,
        definition_line: Option<usize>,
    },
    Position {
        path: String,
        line: usize,
        column: usize,
    },
}
```

Public mappings:

- `code_retrieve.find_references` creates `SymbolSelector`;
- `code_intelligence.references` creates `Position`.

Both routes call one `ReferenceService`.

## 5.3 Always-available fallback

For the target scope:

1. reconcile the workspace snapshot;
2. resolve one declaration or return deterministic ambiguity;
3. iterate all eligible in-memory source files;
4. find the exact identifier with identifier-boundary checks;
5. exclude only the selected declaration occurrence;
6. inspect the Tree-sitter node when supported;
7. classify call/import/type/read/write/other;
8. attach enclosing-symbol context;
9. apply path, production/test, kind and result limits;
10. report truncation and scanned scope.

Do not use the natural-language/general token index as a correctness prefilter.

### Optimization rule

At 300k LOC, benchmark the full in-memory scan first. Add an exact-identifier posting list only when the measured fallback p95 breaches the approved gate.

Any future posting index must have:

- a full-scan oracle;
- per-file incremental replacement;
- cache versioning;
- empty-posting full-scan confirmation;
- exact identifier tokens from syntax or a language-aware lexer;
- no dependency on free-text ranking terms.

## 5.4 Shared result model

```rust
struct SymbolTarget {
    name: String,
    path: String,
    range: SourceRange,
    kind: SymbolKind,
    qualified_name: Option<String>,
}

struct Occurrence {
    path: String,
    range: SourceRange,
    role: OccurrenceRole,
    enclosing_symbol: Option<String>,
    evidence: EvidenceLevel,
}

struct ReferenceResult {
    target: SymbolTarget,
    target_evidence: EvidenceLevel,
    backend: BackendKind,
    freshness: Freshness,
    occurrences: Vec<Occurrence>,
    truncated: bool,
    warnings: Vec<Warning>,
}
```

Separate:

- target identity evidence;
- occurrence classification evidence;
- backend;
- freshness.

A location returned by LSP can be semantically correct while the `call/read/write` label is only syntactically inferred.

## 5.5 Evidence contract

Allowed target evidence:

- `semantic`
- `syntactic`
- `lexical`

Allowed freshness:

- `current`
- `stale`
- `unknown`

Rules:

- only a successful synchronized LSP request may return `semantic`;
- a backend timeout followed by fallback returns fallback evidence plus a warning;
- a semantic zero-result is accepted only from a healthy synchronized server;
- a fallback zero-result includes the scanned file/byte count;
- no error is silently converted into an empty successful result.

---

## 6. LSP architecture

## 6.1 Scope

LSP is genuinely used on repositories such as CrawlerAI, so it remains a production workstream rather than an optional experiment.

Initial tested backends:

- Rust: rust-analyzer;
- Python: basedpyright;
- JavaScript/TypeScript: typescript-language-server.

Generic configuration may exist internally, but only tested backends are claimed as supported.

## 6.2 Split the current intelligence module by responsibility

```text
src/intelligence/
  mod.rs
  service.rs
  worker.rs
  sync.rs
  protocol.rs
  normalize.rs
  workspace_edit.rs
```

Responsibilities:

- `worker`: owns process stdin/stdout and request sequencing;
- `sync`: document version/hash state and open/change/close;
- `protocol`: minimal LSP payload helpers;
- `normalize`: locations, URIs, position conversions;
- `workspace_edit`: rename-preview conversion;
- `service`: routing and fallback policy.

One worker owns each process. No second reader competes for JSON-RPC messages.

## 6.3 Document synchronization

Before a semantic request:

```text
unopened:
  didOpen(version=1, full text)

opened but hash changed:
  didChange(version += 1, full text)
```

Use full-text changes first. They are simpler, correctness-first, and bounded by the existing maximum file size.

On delete/rename:

- close the old URI;
- clear old sync state;
- lazily open the new URI when required.

After restart:

- clear synchronized-document state;
- initialize again;
- reopen lazily.

## 6.4 Capabilities and positions

Record:

- `referencesProvider`;
- `definitionProvider`;
- `renameProvider`;
- sync kind;
- position encoding;
- server name/version;
- initialization duration;
- current readiness and last error.

Public coordinate contract:

- `line`: one-based line number;
- `column`: zero-based UTF-16 code-unit offset for the current compatibility API.

Internally use an explicit `PositionEncoding`. Convert at the LSP boundary and correct the misleading phrase “UTF-16 line.”

Do not add another position parameter.

## 6.5 Semantic routing

`ReferenceService` preference:

1. use a configured, healthy, synchronized LSP backend;
2. return semantic results when successful;
3. fall back only on a classified unsupported/unavailable/timeout/protocol failure;
4. report the fallback reason;
5. do not let the caller choose the backend through a public parameter.

---

## 7. Evaluation architecture

## 7.1 Replace the current evaluator before retrieval changes

Create a runner that exercises the same production operations as the MCP path.

Preferred structure:

```rust
trait RetrievalApi {
    fn execute(&self, operation: RetrievalOperation) -> AppResult<Value>;
}
```

Both:

- `WorkspaceActor::execute_retrieval_operation`, and
- the offline evaluator

call the same production implementation.

Do not benchmark through HTTP for every microbenchmark; use the production service boundary in process. Add a smaller end-to-end MCP suite separately.

## 7.2 Remove the eval-only ranking subsystem after migration

After the live evaluator reaches feature parity for the intended metrics:

- remove `src/index/context.rs`;
- remove `src/index/chunks.rs` unless live retrieval consumes structural chunks;
- remove `ContextParams`, `Ranking`, `SymbolDetail` exports used only by eval;
- remove `FileEntry::chunks` and `FileEntry::path_tf`;
- bump the index-cache schema;
- update cold/warm baselines;
- measure memory reduction.

Preserve the old implementation in Git history rather than keeping a parallel research engine in the production index.

## 7.3 Evaluation suites

### Production retrieval

- file lookup;
- exact symbol;
- literal and regex search;
- outline;
- repository map;
- exact reads;
- scope/path filters;
- handle freshness;
- response truncation.

### References

Rust:

- bare function;
- `self.method()`;
- `value.method()`;
- `Type::associated_function()`;
- trait method and implementation;
- same method name on unrelated types;
- alias/re-export;
- local shadowing;
- field read/write;
- declaration only;
- test-only reference;
- references after edit;
- ambiguous definitions.

Equivalent high-value fixtures for Python and TypeScript.

### Tool surface

Scripted ChatGPT/Claude tasks:

- locate and read implementation;
- inspect references;
- make one-file edit;
- make multi-file edit;
- inspect failed post-apply validation;
- run and poll Bash;
- review Git diff;
- recover from stale handle;
- stage/commit only when requested.

Measure:

- selected tool;
- malformed calls;
- runtime validation errors;
- retries;
- tool calls;
- schema tokens;
- correct final diff.

### Scale

Use deterministic fixtures at:

- approximately 10k LOC;
- approximately 100k LOC;
- approximately 300k LOC.

Use pinned real repositories for relevance and correctness, including CodeWeave and CrawlerAI.

## 7.4 Gates

Freeze absolute targets only after live-path baselines exist.

Initial relative gates:

- no canonical accuracy regression;
- zero false-zero receiver-qualified reference cases;
- fallback exact-name occurrence recall of 100% on deterministic fixtures;
- no fallback result labelled semantic;
- warm live retrieval p95 no worse than 1.10× baseline;
- warm fallback-reference p95 no worse than 1.25× baseline;
- cold index no worse than baseline after removing dead per-file fields;
- memory must improve or remain within 1.05×;
- invalid-call rate must decline after contract changes;
- average schema tokens in the default profile must decline;
- tool calls per workflow must not increase.

---

## 8. Error and capability architecture

## 8.1 Central error registry

Introduce stable internal codes without changing public strings initially:

```rust
enum ErrorCode {
    InvalidArgument,
    MissingOperationField,
    InvalidOperationField,
    StaleSnapshot,
    StaleFile,
    StaleHandle,
    PartialCommit,
    IndexRefreshFailed,
    JournalWriteFailed,
    // ...
}

struct ErrorPolicy {
    code: &'static str,
    retry_kind: RetryKind,
    public_category: ErrorCategory,
}
```

Generate:

- public code string;
- retryability;
- default retry kind;
- documentation;
- tests for uniqueness;
- exhaustive mapping checks.

Call sites may attach cause-specific details, but they do not hand-author retry policy.

Do not collapse distinct recovery paths merely to reduce the number of codes. Consolidate only codes with the same caller action.

## 8.2 Separate static capabilities from dynamic diagnostics

Create one internal `CapabilitySnapshot` source, rendered in two views:

```text
code_capabilities:
  public contracts, supported operations, limits, known limitations

workspace diagnostics:
  current readiness, cache state, generation, snapshot, pending reconcile,
  LSP process state, Bash state, repository metrics
```

During compatibility, retain existing fields as aliases where required, but test that shared values come from the same source.

## 8.3 Truthful edit capability model

Publish:

```json
{
  "atomic_file_replace": true,
  "atomic_multi_file_commit": false,
  "compensating_restore": "best_effort",
  "manual_recovery_possible": true,
  "validation_failures_preserve_edits": true
}
```

Remove ambiguous legacy names such as `supports_rollback_on_failure`.

---

## 9. Internal simplification workstream

## 9.1 Centralize Bash request validation

The Bash allowed-field validation currently exists in both request preparation and manager dispatch.

Create one function:

```rust
fn normalize_bash_request(method: BashMethod, input: &Value) -> AppResult<NormalizedBashRequest>
```

Use it once before dispatch. Manager methods consume typed normalized input. Keep defense-in-depth tests, not duplicate hand-maintained field lists.

## 9.2 Consolidate removed-feature compatibility

Move legacy configuration/tool-name rejection into:

```text
src/compatibility.rs
```

with a table containing:

- removed input;
- replacement;
- removal version;
- error code;
- suggested configuration/tool.

Keep actionable errors, but remove scattered compatibility conditionals.

## 9.3 Share low-level helpers

Move duplicate UTF-8 boundary and response truncation helpers into a small utility module with focused tests.

Do not create a general “utils” dumping ground; functions must have one clear owner such as `text.rs` or `response.rs`.

## 9.4 Reduce WorkspaceActor lock complexity incrementally

Do not rewrite it into a mailbox actor and do not rename it in the same project.

Extract cohesive state owners:

```text
ReconcileState
MutationJournal
RunAttribution
RepositoryStatusCache
```

Each component owns its locks and exposes methods that preserve ordering internally. The top-level workspace object coordinates them without directly combining all guards.

Priority extraction: `RunAttribution`, because it contains complex baseline/completion/frozen-state logic and can be tested independently.

Add concurrency tests for:

- edit + watcher event;
- Bash completion + reconcile;
- concurrent reads during edit;
- cancellation + output polling;
- refresh during pending filesystem events.

## 9.5 Transaction compensation

Keep the current best-effort reverse recovery, but isolate it as:

```text
CommitPlan
CommitProgress
CompensationReport
```

Before write phase:

- prepare all temporary contents;
- confirm destination parents and preconditions;
- only then begin rename/delete application.

This can reduce but not eliminate partial commits.

Responses must preserve:

- applied paths;
- restored paths;
- rollback failures;
- manual recovery requirement;
- current snapshot/reconcile state.

---

## 10. Phased implementation plan

## Phase 0 — live-path evaluation correction

**No production behavior change.**

Deliver:

- public tool/schema snapshot;
- live production retrieval adapter for eval;
- reference fixture framework;
- tool-surface workflow framework;
- 10k/100k/300k scale fixtures;
- baseline cold/warm latency and memory;
- regression reproducing `self.run_edit_validation()` false zero;
- explicit proof that `context()` is not live.

Exit:

- committed live-path baselines;
- temporary evaluator marker established before deletion in Phase 1;
- no implementation optimization started.

## Phase 1 — delete dead retrieval architecture

Deliver:

- migrate all useful eval cases to production operations;
- remove `context.rs`, eval-only ranking types, chunks/path fields when no live consumer remains;
- bump cache schema;
- compare memory and startup;
- keep no parallel v1/v2 engine.

Exit:

- eval and live retrieval use the same code;
- no production regression;
- cold index and memory meet gates.

## Phase 2 — contract and tool-surface cleanup

Deliver:

- typed retrieval/change contract tables;
- runtime required/allowed-field validation;
- generated capability/docs/tests;
- remove `rollback_on_failure` from advertised schemas;
- post-apply validation terminology;
- handle wording cleanup;
- central Bash request normalization;
- static-vs-dynamic capability split;
- initial central error registry.

Exit:

- no generic `query`;
- invalid-call fixture rate improves;
- schema tokens decline;
- compatibility tests pass.

## Phase 3 — correct fallback references

Deliver:

- complete allowed-scope identifier scan;
- identifier-boundary correctness;
- Tree-sitter classification;
- enclosing symbols;
- target/evidence/freshness fields;
- exact deterministic fixtures;
- no general token-index correctness prefilter.

Exit:

- false-zero bug fixed;
- canonical fallback recall gate passes;
- 300k p95 within gate.

## Phase 4 — shared ReferenceService and model

Deliver:

- shared target and occurrence models;
- one serializer;
- route both public reference operations through one service;
- remove duplicate fallback code;
- semantic/fallback routing policy;
- golden response tests.

Exit:

- equivalent location results across both public entry points;
- no duplicate reference engines;
- evidence labels correct.

## Phase 5 — LSP reliability and rust-analyzer

Deliver:

- worker-owned JSON-RPC process;
- capability negotiation;
- position encoding;
- full-text document synchronization;
- restart/reopen behavior;
- rust-analyzer preset;
- existing Python/TypeScript presets migrated;
- current-hash freshness checks;
- Unicode, timeout, restart and post-edit tests.

Exit:

- semantic results after edits are current;
- CrawlerAI and Rust fixtures pass;
- latency baselines recorded;
- fallback remains available and honest.

## Phase 6 — evaluated default tool profile

**Implementation status (2026-07-13): complete.** The non-default `coding` profile exposes 18 tools and reduces the serialized `tools/list` payload from 24,418 to 20,174 bytes (17.4%). ChatGPT and Claude both completed the reversible coding workflow with zero abandoned tasks and no need for a full-only tool. Claude required one client-side tool-loading retry and one correct stale-hash retry; its discovery layer did not surface `code_write`, although the production MCP `tools/list` independently confirmed all 18 tools. `full` remains the default; any default change belongs to a separate versioned release.

Deliver:

- run ChatGPT and Claude workflow suites against current profiles;
- define the coding profile from measured tool use;
- expose uncommon/destructive Git tools only in `full`;
- retain custom profile;
- change the default only in a separate versioned release after connector testing.

Exit:

- fewer visible schema tokens;
- lower or equal malformed-call rate;
- no increase in tool calls or task failures.

## Phase 7 — state ownership and edit-engine simplification

**Implementation status (2026-07-13): complete.** Bash attribution now has one `RunAttribution` owner and one mutex instead of separate baseline/completion maps; compatibility normalization lives in `src/compatibility.rs`; newline/range helpers live only in `workspace::util`; and commit progress, compensation, mutation persistence, and partial-commit reporting are isolated in `workspace::commit`. The workspace lock order is documented, concurrent terminal observers share one frozen attribution result, and the complete test/evaluation gates remain green. Retrieval quality was unchanged on both live fixture suites, and the deterministic 300k-LOC fallback-reference gate passed at 30.365 ms p95 against 250 ms. Cold/warm timing samples varied substantially between consecutive unchanged runs, so no performance-improvement or regression claim is made from those noisy samples.

Deliver:

- extract `RunAttribution`;
- centralize compatibility;
- deduplicate text helpers;
- isolate commit progress/compensation;
- clarify lock ownership;
- add concurrency tests.

Exit:

- no behavior or latency regression;
- fewer multi-lock call sites;
- partial-commit reporting remains complete.

## Phase 8 — measured optimizations only

Candidates:

- exact identifier posting list;
- parsed occurrence cache;
- visibility-derived reference scope;
- LSP call hierarchy;
- faster substring/trigram index.

Adoption requirements:

- approved metric regression exists;
- proposed change fixes that metric;
- correctness oracle remains;
- complexity and memory costs are measured;
- no competing retrieval system remains after adoption.

---

## 11. Findings not adopted as immediate changes

### Delete all narrow edit tools

Rejected. Narrow tools are the simple interface. Their shared engine is a maintenance benefit, not proof the public wrappers are unnecessary.

### Replace flat schemas with `oneOf`

Rejected for now. Hosted-client compatibility is a real constraint. Typed runtime contracts and actionable errors provide most of the value safely.

### Rename WorkspaceActor immediately

Rejected. The name is imperfect, but a rename has no accuracy or latency payoff. Reduce lock ownership and module complexity first.

### Add a graph database because CodeGraph has one

Rejected. CodeGraph's stage separation and provenance are useful; its storage and graph breadth are not justified for 100k–300k LOC.

### Keep the eval-only ranker for possible future use

Rejected after the live evaluator is migrated. Git history is the archive. Parallel unused engines distort memory, startup and architectural decisions.

### Collapse all error codes

Rejected. Centralize and classify them, then consolidate only where the recovery action is identical.

---

## 12. Revert-prevention rules

1. One workstream per commit series.
2. Phase 0 baselines land before behavior changes.
3. Public input schemas have committed snapshots.
4. No API simplification and engine rewrite in the same phase.
5. Every optimization has a simpler correctness oracle.
6. Exact expected paths/ranges/evidence are asserted; not merely `result_count > 0`.
7. Adversarial same-name fixtures precede reference optimization.
8. Compatibility acceptance is separated from advertised schemas.
9. No new dependency without a measured requirement.
10. Do not delete the prior implementation until parity/golden tests pass.
11. Reverts preserve fixtures and baselines that reproduce the failure.
12. Every phase reports accuracy, p50/p95, memory, schema size and workflow call count.
13. No code is described as atomic when it is only compensating.
14. No fallback response is described as semantic.
15. No generic retrieval `query` enters the public contract.

---

## 13. Immediate next slice

The first implementation slice should contain only:

1. a new live-path evaluator adapter;
2. exact reference fixtures, including the receiver-qualified failure;
3. tool-schema snapshots and malformed-call fixtures;
4. memory/LOC/index metrics;
5. a temporary marker around the then-current `context()` evaluator.

This historical slice is complete. Phase 1 subsequently deleted the evaluator, ranker, query sets, and historical baselines; Git history is the only retained archive.

---

## 14. Source basis

### Audit

- `Pasted text(14).txt`, especially:
  - public tool surface and shared edit engine: lines 1–22;
  - concurrency/edit pipeline: lines 24–75;
  - schema confusion risks: lines 77–87;
  - error contract: lines 88–99;
  - duplication and state complexity: lines 100–108;
  - live-vs-eval retrieval finding: lines 109–176.

### Repository verification

Verified during the initial audit:

- live retrieval called `CodeIndex::search`;
- `CodeIndex::context` was called only by eval/tests;
- the same was true before the recent module split;
- `context.rs` was 950 LOC and `chunks.rs` was 192 LOC;
- `chunks` and `path_tf` were built and cached for every file despite having no live consumer;
- Phase 1 deleted those eval-only files and fields rather than retaining a legacy mode;
- Bash field validation is duplicated in `main.rs` and `manager.rs`;
- the UTF-8 boundary helper is duplicated;
- `rollback_on_failure` is still advertised but deprecated and ignored;
- the working tree was clean during review.

### External architecture references retained from v1

- rust-analyzer architecture and find-usages implementation;
- Serena;
- CodeGraph;
- SCIP;
- Kythe;
- Stack Graphs;
- Zoekt;
- ast-grep;
- LSP 3.17 specification.

The revised plan keeps their useful principles while reducing the amount of machinery proposed for CodeWeave.