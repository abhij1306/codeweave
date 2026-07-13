# Contributing to CodeWeave

Thank you for contributing.

## Before opening a change

- Search existing issues and pull requests to avoid duplication.
- Keep the change focused and avoid unrelated formatting churn.
- Do not commit credentials, bearer tokens, private URLs, personal information, generated caches, or machine-specific absolute paths.
- For vulnerabilities, follow the private reporting guidance in [SECURITY.md](SECURITY.md).

## Development setup

```bash
git clone <your-fork-url>
cd codeweave
cp config.example.json config.json
cargo build
cargo test
```

PowerShell uses `Copy-Item config.example.json config.json` instead of `cp`.

Point `workspace.path` at a non-critical test repository. Local `config.json` files are ignored by Git.

## Required checks

Run these before opening a pull request:

```bash
cargo fmt --check
cargo test --release
cargo clippy --all-targets -- -D warnings
cargo build --release
git diff --check
```

## Pull requests

A useful pull request explains:

- the problem and intended behavior;
- the implementation approach;
- tests and validation performed;
- configuration or documentation changes;
- security implications, especially for paths, authentication, commands, edits, or Git operations.

Add or update tests for fixes and new behavior. Do not weaken path canonicalization, authentication, command allow-listing, snapshot checks, edit preconditions, or rollback behavior without a clearly documented security rationale.

Use neutral paths such as `/path/to/project` or `C:\\path\\to\\project` in public examples.

By contributing, you agree that your contributions will be licensed under the MIT License.
