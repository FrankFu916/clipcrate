# Contributing

- `cargo test` — 26 unit + 12 end-to-end CLI tests must pass
- `cargo fmt && cargo clippy --all-targets -- -D warnings` must be clean
- Keep the watcher platform-neutral; platform specifics belong in `src/backend`
- New CLI flags need a test in `tests/cli.rs`
- Update CHANGELOG.md under an "Unreleased" heading

PRs welcome — small and focused first.
