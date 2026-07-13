# eval — production retrieval baseline

The evaluator executes the same explicit operation dispatcher and index search
function used by the MCP `code_retrieve` route:

```text
prepare_retrieval_operation -> execute_index_search -> CodeIndex::search
```

```sh
cargo run -p eval --
cargo run -p eval -- --repo crawlerai
cargo run -p eval -- --repo crawlerai --repo-path ../CrawlerAI
```

Operation fixtures live in [`operations/`](operations/) and baselines are written
to `eval/baseline/live/<repo>.json`.

## What the baseline records

- Recall@1 / @5 / @10 and MRR@10 for explicit retrieval operations.
- Per-operation latency plus p50 and p95.
- A real cold scan and a warm persisted-cache load.
- Indexed file count, total LOC, source LOC, content bytes, token/posting counts,
  symbol counts, and a documented lower bound for owned heap bytes.
- Git revision and dirty-worktree state.
- Known failures as named fixture results rather than hidden aggregate noise.

The CodeWeave fixture requires the receiver-qualified `run_edit_validation`
reference case to pass. The isolated regression fixture in
[`fixtures/references/receiver-qualified-rust/`](fixtures/references/receiver-qualified-rust/)
removes that identifier from the general cached posting list and proves the
complete fallback scan still returns the exact call, syntactic role, and
enclosing symbol.

## 300k fallback reference gate

The scale fixture creates and scans a deterministic 300,000-source-LOC repository.
It runs in the normal test suite and can also be run explicitly with:

```sh
cargo test -p eval --test phase3 -- --nocapture
```

The initial p95 gate is 250 ms and can be overridden for a release environment
with `CODEWEAVE_FALLBACK_300K_P95_GATE_MS`. The benchmark verifies that every
allowed file was scanned and that the exact receiver call was returned.

## Shared reference-service contract

`code_retrieve.find_references` and `code_intelligence.references` use the same
`ReferenceService`, target/occurrence model, serializer, and live in-memory
index. With language servers disabled, their complete fallback responses are
required to be byte-for-byte equivalent. A golden fixture covers both the
fallback and semantic response shapes. Semantic references are labeled current
only after full-text hash/version synchronization and live-index hash checks:

```sh
cargo test -p codeweave-rust --bin codeweave-rust shared_reference
UPDATE_EVAL_SNAPSHOTS=1 cargo test -p codeweave-rust --bin codeweave-rust shared_reference_response_matches_golden
```

## Contract fixtures

`cargo test -p eval --test phase0` checks:

- the committed public `tools/list` schema snapshot;
- malformed `code_retrieve` operation errors;
- the receiver-qualified cached-index full-scan regression.

Regenerate the schema snapshot deliberately with:

```sh
UPDATE_EVAL_SNAPSHOTS=1 cargo test -p eval --test phase0 public_tool_schema_snapshot_matches
```

The retired `CodeIndex::context` evaluator, v1/v2 ranking implementations, query
sets, and historical baselines are intentionally absent. Git history is the only
archive for that deleted architecture.

For CrawlerAI, the default checkout is the sibling `../CrawlerAI`; override it
with `--repo-path` or `CRAWLERAI_REPO`. Use a clean, pinned checkout for release
comparisons. The evaluator stays local and deterministic; it does not simulate a
hosted ChatGPT or Claude client.
