# eval — retrieval + latency baseline

A minimal, offline benchmark for CodeWeave's retrieval engine. It scans either
CodeWeave or the larger CrawlerAI Python/TypeScript repository with the real
`CodeIndex`, runs a fixed query set through `CodeIndex::context`, and reports
retrieval quality and latency.

```sh
cargo run -p eval -- --ranking v1
cargo run -p eval -- --repo crawlerai --ranking v1
cargo run -p eval -- --repo crawlerai --ranking v2
```

CodeWeave remains the default and writes `eval/baseline/v1.json`. CrawlerAI
writes `eval/baseline/crawlerai/v1.json` and `v2.json`, so the two repositories
never overwrite each other's results.

By default, CrawlerAI is resolved at the sibling path `../CrawlerAI`. Override
that with `--repo-path <path>` or the `CRAWLERAI_REPO` environment variable.
The crawler query set records its base Git revision, and every baseline records
the actual revision and whether the worktree was dirty. A revision mismatch or
dirty tree is reported as a warning rather than hidden; use a clean checkout at
the pinned revision for a reproducible release gate.

## What it measures

- **Recall@1 / @5 / @10** — did an expected target path appear in the top-k?
- **MRR@10** — mean reciprocal rank of the first hit.
- **Mean chars** — average payload size per query (token cost proxy).
- **Cold / warm index (ms)** — full scan vs. a second scan of the unchanged tree.
- **Search p50 / p95 (ms)** — per-query `context` latency.

Queries live in [`queries/codeweave.json`](queries/codeweave.json) and
[`queries/crawlerai.json`](queries/crawlerai.json); each lists the
workspace-relative path(s) a good ranker should surface. Queries may also mark
paths as dirty or recently mutated to exercise those ranking signals.

## Why it is deliberately small

An in-process score **cannot** reproduce how the ChatGPT and Claude web clients
actually drive the tools. This benchmark's job is a **trustworthy relative
regression gate** across two representative codebases: re-run both rankings on
the same clean revisions and compare retrieval quality, response size, and
latency. There is intentionally no hosted-client simulation or CI wiring.
