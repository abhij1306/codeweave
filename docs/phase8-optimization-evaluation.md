# Phase 8 Optimization Evaluation

**Date:** 2026-07-13  
**Repository revision:** `fd2bfe97facf93d94729e351e470e1ac9db7efad`

## Decision

Phase 8 is complete for this release with **no production optimization adopted**.

The phase requires an approved, reproducible metric regression before adding an identifier posting list, occurrence cache, visibility-derived scope, call hierarchy, or substring/trigram index. The deterministic reference gates are healthy, retrieval quality is unchanged, and the only adverse signal—a broad CrawlerAI timing increase—was not isolated to a specific algorithm on a controlled A/B runner. Adding index state or a competing retrieval path would therefore violate the phase's adoption requirements.

## Measurement method

- Five serialized CodeWeave live-path evaluator runs.
- Three serialized CrawlerAI live-path evaluator runs.
- Five serialized 300k-LOC fallback-reference runs.
- Generated live baseline files were backed up and restored after measurement.
- Schema bytes were remeasured with the deterministic tool-registry test.
- A detached clean-HEAD A/B build was attempted but exceeded the 120-second command limit before producing measurements. Its temporary worktree was removed, baseline hashes were verified against HEAD, and the incomplete run is excluded from the decision.

All evaluator runs used the production boundary:

```text
prepare_retrieval_operation -> execute_index_search -> CodeIndex::search
```

## Live-path results

### CodeWeave

| Metric | Committed baseline | Current median | Ratio |
|---|---:|---:|---:|
| Recall@1 | 73.7% | 73.7% | 1.000 |
| Recall@5 | 94.7% | 94.7% | 1.000 |
| Recall@10 | 100.0% | 100.0% | 1.000 |
| MRR@10 | 0.827 | 0.827 | 1.000 |
| Cold index | 325.331 ms | 319.997 ms | 0.984 |
| Warm cache | 160.975 ms | 162.625 ms | 1.010 |
| Operation p50 | 0.225 ms | 0.238 ms | 1.058 |
| Operation p95 | 26.056 ms | 25.082 ms | 0.963 |
| Fallback reference p50 | 27.341 ms | 28.453 ms | 1.041 |
| Fallback reference p95 | 42.223 ms | 30.665 ms | 0.726 |

There were zero known-failure misses in every run.

The index heap lower-bound estimate changed from 6,363,275 to a median 6,796,064 bytes while indexed source LOC changed from 29,072 to 29,981. Absolute heap ratio was 1.068; normalized heap per source LOC changed from 218.88 to 226.68 bytes, a ratio of 1.036. No Phase 8 index structure was added.

### CrawlerAI

| Metric | Committed baseline | Current median | Ratio |
|---|---:|---:|---:|
| Recall@1 | 95.7% | 95.7% | 1.000 |
| Recall@5 | 100.0% | 100.0% | 1.000 |
| Recall@10 | 100.0% | 100.0% | 1.000 |
| MRR@10 | 0.978 | 0.978 | 1.000 |
| Cold index | 2,022.189 ms | 3,092.328 ms | 1.529 |
| Warm cache | 1,107.386 ms | 3,106.082 ms | 2.805 |
| Operation p50 | 6.800 ms | 5.715 ms | 0.840 |
| Operation p95 | 35.184 ms | 49.053 ms | 1.394 |

The index heap lower-bound estimate remained exactly 54,973,895 bytes and indexed source LOC remained exactly 161,352. There were zero known-failure misses.

The CrawlerAI slowdown affected file, symbol, and literal operations as well as cold and warm indexing. Literal `search_text` itself was not changed into a different algorithm between the baseline and current implementation. Because the signal is broad and the controlled detached A/B comparison did not finish, it is retained as a runner/measurement investigation rather than approved evidence for a substring or trigram index.

## Deterministic 300k reference gate

Five p95 samples were:

```text
21.759 ms
23.883 ms
23.895 ms
23.410 ms
26.609 ms
```

Median was 23.883 ms and maximum was 26.609 ms against the 250 ms gate. The worst sample consumed 10.6% of the allowed budget. This does not justify an exact-identifier posting list or parsed-occurrence cache.

## Tool surface and workflow guardrails

Deterministic serialized `tools/list` sizes remain:

| Profile | Tools | Bytes |
|---|---:|---:|
| `full` | 26 | 24,418 |
| `coding` | 18 | 20,174 |
| `edit` | 14 | 17,060 |
| `read-only` | 7 | 8,620 |

No production or tool-surface change was adopted in Phase 8, so the connector workflow was not rerun. The last accepted workflow evidence remains unchanged: Claude completed the coding workflow in 26 tool calls with zero abandoned tasks, and both ChatGPT and Claude required no full-only tool.

## Candidate decisions

| Candidate | Decision | Reason |
|---|---|---|
| Exact identifier posting list | Do not adopt | 300k maximum is 26.609 ms against 250 ms; CodeWeave fallback p95 improved. |
| Parsed occurrence cache | Do not adopt | No parsed-occurrence metric regression exists; added memory and invalidation complexity are unjustified. |
| Visibility-derived reference scope | Do not adopt | Accuracy and false-zero correctness gates pass; narrowing scope could reduce recall without a measured requirement. |
| LSP call hierarchy | Do not adopt | Current semantic references and honest fallback satisfy the evaluated workflows; no call-hierarchy task failure is recorded. |
| Faster substring/trigram index | Do not adopt | CrawlerAI timing is broad and not isolated on a controlled A/B runner; no correctness and memory trade-off has been approved. |

## Reopen criteria

Reopen a candidate only after a pinned, controlled runner reproduces an approved gate breach and an alternating before/after benchmark shows that the candidate fixes that metric while preserving exact-path/range/evidence fixtures. Any adopted index must report incremental-update cost, cache-version impact, and heap growth, and must replace rather than coexist with a competing retrieval implementation.
