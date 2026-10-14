# Claude AI Assistant Guide

airmate is a tool for routing AI requests. It is currently a placeholder: the binary prints `Hello, world!`.

## Key files
- `src/main.rs` - binary entry point
- `src/lib.rs` - library code
- `tests/cli.rs` - runs the built binary
- `.github/workflows/` - CI (`ci.yml`), commit lint, release

## Workflow
- Run `./scripts/build.sh` before committing; it mirrors CI (build, fmt, clippy, tests, docs).
- Conventional commits, with scopes from `.omni-dev/scopes.yaml`; check with
  `omni-dev git commit message lint origin/main..HEAD`.
- Keep dependencies minimal. `cargo deny` and `cargo audit` run in CI.
- Add user-visible changes under `[Unreleased]` in `CHANGELOG.md`.
