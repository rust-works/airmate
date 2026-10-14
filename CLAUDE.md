# Claude AI Assistant Guide

airmate is a local, default-closed router for AI requests: it translates between the Anthropic, OpenAI
(Chat and Responses), Gemini and Bedrock APIs and routes across upstreams. See `README.md` for what it
does and `docs/` for the operator guide, the translation reference and the decision records.

## Key files
- `airmate-core/` - the sans-I/O translation core (IR, adapters, field policy, signatures, streams). No
  I/O of any kind: `scripts/check-core-deps.sh` enforces it.
  - `src/policy/` - the field policy tables; `src/inbound/` client parsers; `src/outbound/` upstream
    builders and parsers; `src/stream/` renderers and the sequencer; `src/adapters.rs` dispatch.
- `src/` - the gateway: `config/`, `server/` (listener, guard, generate), `router/`, `usage/`, `cli/`,
  plus `admin.rs`, `metrics.rs`, `otel.rs`, `doctor.rs`, `service.rs`, `sys.rs` (all the `unsafe`).
- `tests/` - integration tests against a scripted mock upstream (`tests/support`); `tests/cli.rs` runs
  the built binary; `tests/live.rs` is env-gated live conformance; `tests/perf.rs` is the budget.
- `tools/drift/` - drift checks against provider API definitions; `fuzz/` - `cargo fuzz` targets.
- `.github/workflows/` - CI (`ci.yml`), commit lint, release, and the scheduled drift, fuzz and perf jobs.

## Workflow
- Run `./scripts/build.sh` before committing; it mirrors CI (build, fmt, clippy, tests, docs).
- Conventional commits, with scopes from `.omni-dev/scopes.yaml`; check with
  `omni-dev git commit message lint origin/main..HEAD`.
- Keep dependencies minimal and justify each in `Cargo.toml`. `cargo deny` and `cargo audit` run in CI.
- Add user-visible changes under `[Unreleased]` in `CHANGELOG.md`.
- Adding a request field: the policy table row and the parser case go in one change; regenerate the docs
  with `UPDATE_DOCS=1 cargo test --test docs`.
- Never silently drop a field that changes behaviour: translate it, ignore it *and report it*, or reject it.
- Secrets are `Secret`s: never `Display`ed or `Debug`ged, never in a log line, a metric label or an error
  message (upstream error text is scrubbed). `tests/leaks.rs` guards this.
