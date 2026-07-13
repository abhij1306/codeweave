# CodeWeave Architecture Improvement Plan

**Status:** Proposed design freeze  
**Target repository size:** small and mid-sized repositories, up to approximately **300,000 source lines of code**  
**Primary product metrics:** **accuracy** and **latency**  
**Implementation rule:** no production refactor begins until the Phase 0 baselines and this contract are approved.

---

## 1. Executive decision

CodeWeave should remain a **small, local, single-repository code service**, not evolve into a compiler, graph database, search cluster, or embedded coding agent.

The target architecture has three deliberately separate capabilities:

```text
                         CodeWeave public tools
                                  |
                  +---------------+----------------+
                  |                                |
       explicit retrieval operations       position-based intelligence
            code_retrieve                    code_intelligence
                  |                                |
                  +---------------+----------------+
                                  |
                         ReferenceService
                                  |
              +-------------------+-------------------+
              |                                       |
      always-available fallback                 optional LSP backend
    in-memory exact-name scan +                 rust-analyzer / Python /
       tree-sitter classification                 TypeScript servers
              |                                       |
       syntactic / lexical evidence              semantic evidence
```

The key architectural principle, borrowed from rust-analyzer, is:

> Fast lexical candidate discovery is allowed only when it is a **recall-safe superset**. Semantic resolution verifies identity when a semantic backend is available.

For CodeWeave's 300k-LOC ceiling, the simplest recall-safe fallback is initially a **full scan of the already in-memory eligible source files for an exact identifier**, followed by tree-sitter classification. This is preferable to adding an identifier database before an evaluation proves that the scan is too slow.

### Frozen decisions

1. **No generic public `query` parameter is introduced.**
2. `code_retrieve` keeps explicit discriminated operations such as `find_symbol`, `search_text`, `find_references`, and `read`.
3. `code_intelligence` remains position based: `path + line + column`.
4. Both public routes eventually share one internal `ReferenceService`.
5. A healthy LSP is the authority for semantic identity.
6. Tree-sitter and lexical fallback results remain explicitly labelled as non-semantic.
7. Reference correctness must never depend on the general natural-language/token-ranking index.
8. The first fallback implementation scans the complete allowed in-memory scope; an identifier posting index is added only if the 300k-LOC latency evaluation requires it.
9. No SQLite, FTS5, graph database, SCIP serialization, Salsa/HIR clone, Stack Graph implementation, or mandatory embeddings.
10. Every retrieval or reference change must ship with accuracy and latency deltas from committed eval fixtures.

---

## 2. Product scope

### 2.1 Supported scale

CodeWeave is optimized for one repository at a time with:

- approximately 0–300k source LOC;
- source files available on a local filesystem;
- one eager in-memory index;
- incremental refresh after edits;
- optional persistent cache for faster restart;
- a small number of language-server processes, enabled only when configured.

The 300k-LOC figure is an **optimization target**, not initially a hard runtime rejection. The workspace summary should report measured source LOC and file count so evaluations and diagnostics can classify the repository size.

### 2.2 Primary metrics

In order:

1. **Accuracy**
   - correct target;
   - no false-zero reference results caused by indexing;
   - honest semantic/syntactic/lexical evidence;
   - correct behavior after edits;
   - deterministic ambiguity handling.

2. **Latency**
   - cold index time;
   - warm startup time;
   - warm retrieval p50/p95;
   - warm reference p50/p95;
   - LSP cold readiness and first-result time;
   - edit-to-fresh-reference latency.

Secondary guardrails are response size, memory, and agent tool-call count.

### 2.3 Non-goals

CodeWeave will not:

- implement Rust name resolution, trait solving, macro expansion, or type inference itself;
- support monorepo-scale distributed indexing;
- build a framework-aware relationship graph in the initial architecture;
- expose an open-ended natural-language retrieval parameter;
- make semantic backends mandatory;
- infer that a syntactic same-name match is a semantic reference;
- add a new storage engine without a measured p95 or memory need;
- refactor editing, Git, Bash, and retrieval simultaneously.

---

## 3. Research synthesis

## 3.1 rust-analyzer — the main reference

rust-analyzer separates syntax, semantic analysis, IDE APIs, VFS, and LSP transport. Its find-usages implementation follows a particularly relevant rule:

1. resolve the symbol under a position to a semantic definition;
2. derive a safe search scope from visibility and the crate graph;
3. run fast text search to obtain a **superset** of occurrences;
4. parse each occurrence and use precise name resolution to confirm it targets the definition.

It contains specialized fast paths, but they are explicitly designed to allow false positives while avoiding false negatives, after which semantic verification is still performed.

**What CodeWeave should copy**

- position/definition identity before semantic reference lookup;
- text search as a candidate accelerator, never as semantic proof;
- a clear API boundary between transport and analysis;
- consistent file snapshots and freshness;
- explicit search scope;
- incremental invalidation limited to changed files;
- serializable, tool-facing result models that do not expose backend internals.

**What CodeWeave should not copy**

- Salsa;
- Rust HIR;
- crate-graph/name-resolution implementation;
- macro expansion;
- type inference;
- rust-analyzer's compiler-scale internal decomposition.

Those belong in rust-analyzer itself and should be consumed through LSP.

## 3.2 Serena

Serena demonstrates an agent-facing symbolic toolset backed primarily by language servers. Its strongest relevant choices are:

- symbol-level operations rather than forcing the agent to reconstruct relationships from file reads;
- position-based semantic requests;
- separate basic file/pattern tools for situations where semantics are unnecessary;
- a backend abstraction rather than embedding one language implementation in the MCP layer.

**Borrow:** the semantic tool philosophy and backend isolation.  
**Do not borrow:** the full multi-language integration scope, memory subsystem, or configuration surface.

## 3.3 CodeGraph

CodeGraph separates:

```text
extraction -> storage -> resolution -> graph queries -> AI context
```

It extracts tree-sitter nodes and edges, persists them in SQLite/FTS5, resolves imports/calls/framework patterns, and maintains freshness incrementally. It also records fixed node/edge kinds and provenance.

**Borrow**

- extraction, resolution, and presentation are separate stages;
- relationships carry provenance;
- stale index state must never be silently presented as current;
- one response should provide useful context and reduce follow-up reads;
- incremental refresh and connect-time catch-up are product features, not implementation details.

**Do not borrow yet**

- SQLite/FTS5;
- a persistent call graph;
- framework-specific route resolvers;
- dozens of node and edge kinds;
- graph traversal APIs.

Those are valuable at larger scales and for architecture/impact features, but unnecessary for fixing CodeWeave's reference correctness at 300k LOC.

## 3.4 SCIP

SCIP's useful abstraction is a language-neutral index made of documents, symbol information, occurrences, roles, and relationships. It explicitly allows indexers to exist on a spectrum from compiler-precise to heuristic syntax-directed analysis.

**Borrow:** a small internal `Occurrence` and `SymbolTarget` model, plus explicit evidence.  
**Do not borrow:** protobuf, global symbol descriptors, package identity, streaming index files, or external-index merging.

## 3.5 Kythe

Kythe separates anchors/locations, semantic nodes, and typed edges and includes verifier tooling for indexer correctness.

**Borrow:** location anchors and verification-first thinking.  
**Do not borrow:** compilation extraction, serving tables, distributed xref infrastructure, or its full schema.

## 3.6 Stack Graphs

Stack Graphs encode language-specific name-resolution rules incrementally without requiring a compiler. This proves syntax-directed semantic resolution is possible, but it requires substantial per-language rule systems. The GitHub repository is also archived.

**Decision:** study only as background. Do not implement Stack Graphs in CodeWeave.

## 3.7 Zoekt

Zoekt is optimized for very fast substring and regex search using trigram indexing and symbol signals.

**Decision:** retain as a future benchmark reference. At 300k LOC, CodeWeave must first prove that its existing in-memory text index or direct byte scanning is insufficient before adopting trigram storage.

## 3.8 ast-grep

ast-grep shows how tree-sitter can provide fast structural matching and rewriting.

**Borrow:** AST-node-aware identifier and occurrence classification.  
**Do not claim:** structural matching is semantic name resolution.

## 3.9 LSP specification

The LSP reference request is defined by a text document position, not a symbol-name string. Correctness also depends on:

- negotiated capabilities;
- negotiated position encoding;
- synchronized document contents;
- monotonically updated document versions;
- `didOpen`, `didChange`, and `didClose`;
- handling null results separately from protocol errors.

This means CodeWeave's LSP architecture must include document synchronization, not only process startup and request dispatch.

---

## 4. Current CodeWeave findings

### 4.1 What is already good

- one repository per process;
- eager index and watcher;
- explicit retrieval operations with flat schemas;
- no public generic `query`;
- exact reads and provenance handles;
- evidence labels;
- optional LSP boundary;
- persistent supervised LSP processes;
- an offline retrieval/latency eval crate;
- per-tool latency samples;
- incremental file refresh;
- narrow edit tools and one transactional engine.

These should remain.

### 4.2 Reference correctness defect

The current indexed reference path:

1. resolves a symbol definition;
2. tokenizes the symbol name;
3. uses the **general text token index** to choose candidate files;
4. scans only those files with an exact identifier regex.

For:

```rust
self.run_edit_validation(...)
```

the caller file was indexed with the compound token `self.run_edit_validation` and split words, but not the exact posting `run_edit_validation`. The candidate stage discarded the file before the exact regex could inspect it.

The defect is architectural: the ranking/token index became a correctness boundary for references.

### 4.3 LSP reliability gaps

The current semantic service is a useful prototype, but it mixes:

- process supervision;
- JSON-RPC request/response handling;
- document opening;
- URI conversion;
- semantic requests;
- fallback scanning;
- WorkspaceEdit normalization;
- tests.

It currently has hardcoded Python and TypeScript clients. A document is opened once, but the architecture does not yet establish a complete content-version synchronization contract after CodeWeave edits. Rust is not configured as a semantic backend, even though rust-analyzer is the natural semantic authority for Rust.

### 4.4 Eval gap

The existing eval crate measures ranked context retrieval but does not provide a dedicated reference benchmark. Current reference unit tests emphasize bare calls and therefore did not expose receiver-qualified calls.

---

## 5. Frozen public API

No public tool multiplication or generic query field is required for this plan.

## 5.1 `code_retrieve`

Keep the current explicit batch model:

```json
{
  "operations": [
    {
      "operation": "find_references",
      "symbol": "run_edit_validation",
      "definition_path": "src/workspace/validation.rs",
      "definition_line": 14,
      "reference_scope": "all",
      "reference_kinds": ["call"],
      "max_results": 20,
      "context_lines": 2
    }
  ]
}
```

Rules:

- `symbol` is a symbol selector, not free-form prose.
- `definition_path` and `definition_line` disambiguate.
- no `query`, `mode`, `strategy`, or `backend` parameter is added;
- backend selection is internal;
- path filters and scope remain explicit;
- batching remains the mechanism for reducing MCP round trips.

## 5.2 `code_intelligence`

Keep position-based operations:

```json
{
  "operation": "references",
  "path": "src/workspace/validation.rs",
  "line": 14,
  "column": 24,
  "max_results": 20
}
```

This route may start from either a declaration or a usage. It resolves the target at the position before searching.

## 5.3 Shared response contract

Both routes should normalize to the same reference payload:

```json
{
  "target": {
    "name": "run_edit_validation",
    "path": "src/workspace/validation.rs",
    "start_line": 14,
    "start_column": 24,
    "kind": "method"
  },
  "evidence": "semantic",
  "backend": "rust-analyzer",
  "scope": "all",
  "freshness": "current",
  "snapshot_id": "snap_...",
  "result_count": 1,
  "truncated": false,
  "results": [
    {
      "path": "src/workspace/edit.rs",
      "start_line": 164,
      "start_column": 17,
      "end_line": 164,
      "end_column": 36,
      "reference_kind": "call",
      "classification_evidence": "syntactic",
      "enclosing_symbol": "WorkspaceActor::code_edit",
      "handle": "range:..."
    }
  ],
  "warnings": []
}
```

Notes:

- `evidence` describes target resolution: `semantic`, `syntactic`, or `lexical`.
- `classification_evidence` separately describes how `call`, `read`, `write`, etc. was assigned. LSP references normally return locations, not usage categories.
- `freshness` is `current`, `stale`, or `unknown`.
- `truncated` is mandatory.
- a zero-result semantic response from a healthy synchronized backend is materially stronger than a zero-result lexical response.
- existing response fields can be retained during migration; new fields are additive.

---

## 6. Target internal architecture

```text
WorkspaceManager
  |
  +-- WorkspaceActor
  |     +-- FileState / watcher / snapshot
  |     +-- CodeIndex
  |     |     +-- TextIndex
  |     |     +-- SymbolIndex
  |     |     +-- line/range metadata
  |     +-- edit, git and bash services (unchanged)
  |
  +-- AnalysisServices
        +-- ReferenceService
        |     +-- SemanticReferenceBackend
        |     +-- FallbackReferenceBackend
        +-- LspManager
              +-- one worker per enabled language server
              +-- document synchronization state
              +-- capability and position-encoding state
```

The manager remains the composition root. The workspace actor remains responsible for the repository snapshot and index. `AnalysisServices` is introduced narrowly and does not absorb editing, Bash, or Git.

## 6.1 Shared data model

Introduce small plain Rust structs:

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

enum EvidenceLevel {
    Semantic,
    Syntactic,
    Lexical,
}

enum OccurrenceRole {
    Declaration,
    Call,
    Import,
    Type,
    Read,
    Write,
    Other,
}

struct ReferenceResult {
    target: SymbolTarget,
    evidence: EvidenceLevel,
    backend: String,
    freshness: Freshness,
    occurrences: Vec<Occurrence>,
    truncated: bool,
    warnings: Vec<Warning>,
}
```

This is intentionally much smaller than SCIP or Kythe. It exists so LSP and fallback paths cannot drift into incompatible outputs.

## 6.2 `ReferenceService`

Inputs are internal target types, not a generic query:

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

Algorithm:

1. ensure the CodeWeave snapshot is reconciled;
2. resolve the target:
   - symbol selector through the symbol index;
   - position through LSP when enabled, otherwise the exact tree-sitter name/name-reference node;
3. check whether a configured semantic backend supports references for the language;
4. if yes, synchronize the relevant document and issue `textDocument/references`;
5. if semantic lookup succeeds, normalize locations and return semantic evidence;
6. if the backend is unavailable, unsupported, timed out, or unhealthy, run the fallback;
7. return the fallback reason explicitly;
8. never silently convert a semantic error into a semantic-looking result.

## 6.3 Fallback reference algorithm

### Phase 1 baseline

For the allowed scope:

1. iterate all eligible in-memory `FileEntry` values;
2. use a byte substring finder for the exact identifier;
3. enforce identifier boundaries;
4. exclude only the selected declaration range;
5. classify the syntax node or line as call/import/type/read/write/other;
6. create handles and enclosing-symbol metadata;
7. apply requested scope, kind filter, max results, and truncation;
8. report `evidence: syntactic` when a parser confirms an identifier node, otherwise `lexical`.

No general token-index prefilter is used.

This is simple, recall-safe for exact identifier spellings, and likely fast enough for 300k LOC because file contents are already in memory. It must be benchmarked rather than guessed.

### Optional optimization, only after evidence

If warm fallback p95 breaches the approved 300k-LOC target, add:

```rust
HashMap<InternedIdentifier, SmallVec<OccurrenceLocation>>
```

The posting list must be built from exact identifier tokens, not natural-language terms. It is an accelerator only. Correctness rules:

- any unsupported grammar or inconsistent posting triggers full-scope scan;
- an empty posting cannot produce a final zero without a full-scope confirmation;
- cache schema is versioned;
- incremental replacement removes and rebuilds postings for only the changed file;
- tests compare indexed results against full-scan results.

Do not implement this optimization in Phase 1.

## 6.4 Semantic backend

The LSP backend is authoritative only when all are true:

- server is configured and initialized;
- `referencesProvider` is supported;
- the document has been synchronized to the current CodeWeave content hash;
- position encoding is known;
- the response belongs to the active request;
- all returned file URIs are within the workspace;
- no response normalization error occurred.

Otherwise the response is fallback evidence with an explicit warning.

---

## 7. LSP architecture

## 7.1 Configuration

Replace hardcoded internal Python/TypeScript ownership with a language-keyed normalized configuration while preserving compatibility during migration:

```json
{
  "intelligence": {
    "servers": {
      "rust": {
        "enabled": true,
        "command": "rust-analyzer",
        "args": [],
        "timeoutMs": 10000
      },
      "python": {
        "enabled": false,
        "command": "basedpyright-langserver",
        "args": ["--stdio"]
      },
      "typescript": {
        "enabled": false,
        "command": "typescript-language-server",
        "args": ["--stdio"]
      }
    }
  }
}
```

A built-in registry supplies language IDs, file extensions, default command/arguments, and tested capabilities. Do not expose extensions or protocol details unless a real language integration requires an override.

Initial tested presets:

- Rust: rust-analyzer;
- Python: basedpyright;
- JavaScript/TypeScript: typescript-language-server.

Do not claim broad language support merely because the configuration is generic.

## 7.2 One worker per server

Use a small worker abstraction:

```text
caller -> bounded command channel -> LSP worker owns process
                                  -> sequential request dispatch
                                  -> message pump
                                  -> response or timeout
```

The worker exclusively owns stdin/stdout and process state. This avoids multiple functions competing to read JSON-RPC messages. Sequential requests are sufficient for the target scale and simpler than a fully concurrent multiplexer. Notifications are consumed and recorded by the same worker.

State:

- process and restart count;
- server capabilities;
- negotiated position encoding;
- supported document sync kind;
- open documents;
- per-document version and content hash;
- last error;
- readiness;
- startup/indexing timestamps.

## 7.3 Document synchronization

Before any semantic request:

```text
if unopened:
    didOpen(version=1, full text)
else if current CodeWeave hash != synchronized hash:
    didChange(version += 1, full text)
```

Full-text `didChange` is deliberately selected first. At 300k repository LOC, individual files are still bounded by `max_file_bytes`, and full sync is much simpler and safer than computing incremental edits. Incremental LSP changes can be evaluated later.

On rename/delete:

- `didClose` old URI;
- issue file notifications only if required by the server;
- open the new URI when queried.

After process restart:

- clear open-document state;
- reinitialize;
- reopen lazily.

Semantic results include the document hash/snapshot against which they were requested.

## 7.4 Position encoding

Negotiate and store UTF-8/UTF-16/UTF-32 position encoding. The current public `column` definition should remain stable, but conversion must occur at the LSP boundary. Do not assume UTF-16 forever simply because it is the protocol default.

## 7.5 Rust support

Add rust-analyzer as the first new semantic preset because it is both the affected language and the strongest semantic implementation studied.

Do not imitate rust-analyzer's HIR. CodeWeave's responsibilities are:

- start it;
- synchronize files;
- send position-based requests;
- normalize locations;
- expose latency, readiness, and failure reasons.

---

## 8. Retrieval architecture beyond references

## 8.1 Keep indexes purpose-specific

```text
TextIndex
  purpose: literal/regex/natural-language discovery and ranking
  correctness: ranked retrieval, not semantic identity

SymbolIndex
  purpose: declarations, outlines, exact symbol selectors
  correctness: syntax-derived declaration locations

Reference fallback
  purpose: exact same-spelling occurrence discovery
  correctness: complete scan of allowed scope, identity only best-effort

LSP
  purpose: semantic definition/reference/rename identity
  correctness: delegated to language server
```

Do not reuse one index merely because it already contains strings.

## 8.2 Keep retrieval batching

CodeGraph's useful product lesson is that reducing tool calls matters. CodeWeave already obtains this without a graph database by allowing up to 12 explicit operations in one `code_retrieve` call.

Continue to improve batching and response context rather than adding a natural-language planner to the server.

A separate high-level exploration tool should be considered only after workflow evals demonstrate that explicit batching cannot meet tool-call targets. It is not part of this plan.

## 8.3 Freshness

Every indexed and semantic result should expose enough state to detect staleness:

- `snapshot_id`;
- current file hash for handles;
- `reconcile_pending`;
- semantic `freshness`;
- fallback reason if LSP is behind or unavailable.

A quiet stale answer is worse than a slower honest answer.

---

## 9. Code organization plan

Do not rewrite the repository. Use a strangler-style migration.

### Existing files retained

- `src/index/*` for text, symbol, file metadata, and fallback scan primitives;
- `src/workspace/retrieve.rs` for public operation parsing;
- `src/manager.rs` as composition root;
- edit, Bash, Git, watcher, journal, and transport modules unchanged.

### New/split modules

```text
src/references/
  mod.rs           ReferenceService and routing
  model.rs         SymbolTarget, Occurrence, ReferenceResult
  fallback.rs      full-scope exact identifier scan
  normalize.rs     public JSON normalization

src/intelligence/
  mod.rs           Semantic service public boundary
  worker.rs        owned LSP process and message loop
  sync.rs          open/change/close and version/hash state
  lsp_types.rs     minimal request/response helpers
  workspace_edit.rs existing rename normalization logic
```

Migration steps:

1. move no behavior initially; extract tests around existing behavior;
2. introduce shared model;
3. migrate fallback;
4. migrate LSP worker;
5. route both public operations to the service;
6. delete duplicate fallback logic only after parity tests.

Avoid renaming unrelated modules during these phases.

---

## 10. Evaluation architecture

Accuracy and latency gates are part of the feature, not a follow-up.

## 10.1 Extend the existing eval crate

Add subcommands:

```text
cargo run -p eval -- retrieval ...
cargo run -p eval -- references --backend fallback ...
cargo run -p eval -- references --backend semantic --language rust ...
cargo run -p eval -- freshness ...
```

Keep existing baseline files; add:

```text
eval/
  reference-fixtures/
    rust/
    python/
    typescript/
  reference-cases/
    rust.json
    python.json
    typescript.json
  baseline/
    references/
      fallback.json
      rust-analyzer.json
  workflow-runs/
```

## 10.2 Canonical accuracy cases

### Rust

- bare free-function call;
- `self.method()` receiver call;
- `value.method()` receiver call;
- `Type::associated_function()`;
- trait method and implementation;
- same method name on unrelated types;
- imported alias;
- re-export;
- local variable shadowing;
- field read and write;
- constructor usage;
- macro-generated or macro-invoked reference;
- test-only reference;
- declaration-only symbol;
- references after an edit;
- ambiguous same-name declarations requiring `definition_path`.

### Python

- module function import;
- aliased import;
- instance method;
- class method;
- same method name on unrelated classes;
- local shadowing;
- re-export;
- reference after edit.

### TypeScript

- named import and alias;
- class method;
- interface/implementation;
- same-name methods;
- namespace/module re-export;
- property read/write;
- reference after edit.

## 10.3 Accuracy metrics

For semantic backends:

- precision;
- recall;
- F1;
- false-zero rate;
- wrong-target rate;
- ambiguous-target handling rate;
- duplicate result rate;
- stale-result rate after edit.

For fallback:

- exact-name occurrence recall;
- false-zero rate;
- same-name false-positive rate;
- classification accuracy;
- evidence-honesty rate;
- full-scan/index parity if an accelerator is later added.

For general retrieval:

- existing Recall@1/5/10 and MRR;
- complete-symbol rate;
- mean response chars.

## 10.4 Latency metrics

Measure separately:

- cold CodeWeave index;
- warm CodeWeave startup;
- warm literal/symbol retrieval p50/p95;
- warm fallback references p50/p95;
- LSP process startup;
- LSP project-ready/first-useful-result time;
- warm semantic references p50/p95;
- single-file index refresh p50/p95;
- edit-to-fresh-fallback-result p50/p95;
- edit-to-fresh-semantic-result p50/p95.

Run at three size buckets:

- small: approximately 10k LOC;
- medium: approximately 100k LOC;
- maximum target: approximately 300k LOC.

Use real pinned repositories for accuracy and deterministic generated repositories for scale curves.

## 10.5 Initial gates

Absolute SLOs should be frozen only after Phase 0 measures the current implementation on the target machine. Initial relative gates:

- no canonical reference case may regress;
- receiver-qualified calls must have zero false-zero results;
- fallback exact-name recall must be 100% on canonical fixtures;
- semantic precision and recall must be 100% on deterministic fixtures supported by the language server;
- warm fallback reference p95 must be no worse than 1.25× the approved baseline;
- warm general retrieval p95 must be no worse than 1.10×;
- cold startup must not regress by more than 10% unless accuracy improves and the trade-off is explicitly approved;
- post-edit freshness must be correct before a response is labelled `current`;
- no result may be labelled semantic after fallback.

After Phase 0, add absolute 300k-LOC targets based on measured hardware rather than estimates.

---

## 11. Phased implementation plan

## Phase 0 — freeze and baseline

**Production behavior:** unchanged.

Deliver:

- commit this architecture decision;
- add reference fixture definitions and gold results;
- add 10k/100k/300k generated scale fixtures;
- measure current false-zero, precision/recall, and p50/p95;
- capture rust-analyzer cold and warm behavior;
- record current memory as a guardrail;
- add a test that reproduces `self.run_edit_validation()` returning zero.

Exit criteria:

- baseline files committed;
- public schema snapshot committed;
- design decisions approved;
- no implementation patch mixed into the phase.

## Phase 1 — correct always-available fallback

Change only the fallback:

- remove general token-index candidate filtering from reference lookup;
- scan every eligible in-memory file with exact identifier boundaries;
- preserve scope, kind filters, result limits, handles, and evidence;
- add receiver, associated-function, same-name, and declaration-only tests;
- add `truncated` and complete scanned-scope metadata if absent.

Do not add an identifier posting index.

Exit criteria:

- false-zero regression fixed;
- fallback exact-name recall 100%;
- latency within gate at 300k LOC;
- general retrieval baselines unchanged.

## Phase 2 — shared reference model

- introduce `SymbolTarget`, `Occurrence`, and `ReferenceResult`;
- normalize current indexed and intelligence responses through one serializer;
- separate target-resolution evidence from occurrence-kind classification;
- keep public fields backward compatible;
- move duplicate reference classification into one module.

Exit criteria:

- golden JSON tests for both public routes;
- no schema input changes;
- no accuracy or latency regression.

## Phase 3 — reliable generic LSP worker

- split `intelligence.rs`;
- create one owner worker per configured server;
- record capabilities and position encoding;
- implement full-text didOpen/didChange/didClose synchronization;
- track version + content hash;
- add rust-analyzer preset;
- retain Python and TypeScript compatibility;
- test process restart, timeout, stale file, Unicode columns, rename, and outside-root URI rejection.

Exit criteria:

- semantic queries after CodeWeave edits use current content;
- no competing stdout reader;
- rust-analyzer deterministic fixture passes;
- cold/warm latency captured.

## Phase 4 — one internal `ReferenceService`

- route `code_intelligence.references` through `ReferenceService`;
- route `code_retrieve.find_references` through the same service after it resolves the declaration selector;
- prefer a healthy semantic backend automatically;
- fallback on explicit, classified failures;
- expose backend, evidence, freshness, and warning;
- do not expose a backend-selection parameter.

Exit criteria:

- both public routes produce equivalent locations for the same semantic target;
- no duplicate fallback implementations;
- an LSP outage returns honest fallback evidence;
- no public `query` field appears.

## Phase 5 — measured optimizations only

Evaluate:

1. exact identifier posting list;
2. parsed occurrence cache;
3. safe visibility-derived scopes;
4. richer enclosing-symbol context;
5. call hierarchy operations exposed through LSP.

Adopt an item only when:

- it improves approved latency or workflow metrics;
- exact accuracy does not regress;
- maintenance complexity is documented;
- a full-scan correctness oracle remains in tests.

SQLite, trigram indexes, persistent graphs, and framework resolution remain out of scope unless the 300k-LOC evaluation clearly invalidates the in-memory design.

---

## 12. Revert-prevention rules

This area has already suffered repeated reversions, so the implementation process must reduce ambiguity.

1. One phase per commit series.
2. No API redesign and engine rewrite in the same phase.
3. Before each phase:
   - clean Git status;
   - baseline recorded;
   - exact files and invariants listed.
4. Every optimization has a correctness oracle:
   - fallback full scan is the oracle for identifier postings;
   - LSP deterministic fixture is the oracle for semantic routing.
5. No test should assert only `result_count > 0`; tests assert exact paths, ranges, evidence, and target.
6. Add adversarial same-name cases before performance optimization.
7. Do not delete the old path until the new path passes golden parity.
8. No configuration migration without backward-compatibility tests.
9. No new dependency unless a standard-library/current-crate implementation fails the measured target.
10. Any rollback of a phase must preserve fixture and baseline additions so the failure remains reproducible.

---

## 13. Final architecture recommendation

The appropriate architecture for CodeWeave is not a miniature rust-analyzer or CodeGraph. It is:

- an eager in-memory file/text/symbol index;
- exact deterministic retrieval operations;
- a complete in-memory identifier scan as the reliable fallback;
- tree-sitter for syntax structure and occurrence classification;
- LSP for semantic identity;
- one shared result model;
- one shared reference service;
- explicit evidence and freshness;
- eval-gated optimization for a 300k-LOC ceiling.

This preserves CodeWeave's main advantage: a small, understandable, low-latency local service with high-confidence behavior and narrow public contracts.

---

## 14. Primary sources studied

- [rust-analyzer architecture](https://rust-analyzer.github.io/book/contributing/architecture.html)
- [rust-analyzer find-usages search implementation](https://raw.githubusercontent.com/rust-lang/rust-analyzer/master/crates/ide-db/src/search.rs)
- [rust-analyzer reference entry point](https://raw.githubusercontent.com/rust-lang/rust-analyzer/master/crates/ide/src/references.rs)
- [Serena](https://github.com/oraios/serena)
- [CodeGraph](https://github.com/colbymchenry/codegraph)
- [CodeGraph: how it works](https://colbymchenry.github.io/codegraph/core-concepts/how-it-works/)
- [CodeGraph: knowledge graph](https://colbymchenry.github.io/codegraph/core-concepts/knowledge-graph/)
- [CodeGraph: indexing](https://colbymchenry.github.io/codegraph/guides/indexing/)
- [SCIP](https://github.com/scip-code/scip)
- [SCIP protocol](https://raw.githubusercontent.com/scip-code/scip/main/scip.proto)
- [Kythe](https://github.com/kythe/kythe)
- [Stack Graphs](https://github.com/github/stack-graphs)
- [Zoekt](https://github.com/sourcegraph/zoekt)
- [ast-grep](https://github.com/ast-grep/ast-grep)
- [LSP 3.17 specification](https://microsoft.github.io/language-server-protocol/specifications/lsp/3.17/specification/)
