# Contributing

## Before you push

```bash
./scripts/build.sh
```

This runs `cargo build`, `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test` and `cargo doc` with warnings denied, which are the same checks CI runs.

## Layout

- `airmate-core/` is the sans-I/O translation core: the IR, protocol adapters, the field policy tables,
  signatures and stream state machines. It does no I/O (no sockets, clock, randomness or filesystem);
  `scripts/check-core-deps.sh` and the *Core Purity* CI job keep its dependency tree free of the crates
  that would bring them in.
- `src/` is the gateway: configuration, the server, the router, observability, the CLI.
- `tools/drift/` compares provider API definitions with the policy tables; `fuzz/` holds the `cargo fuzz`
  targets; `docs/` is the operator guide, the translation reference and the decision records.

## Adding a field, a protocol or a provider

- **A field.** Add its row to the policy table in `airmate-core/src/policy/` *and* its handling in the
  parser, in one change, with a test. A row classed *translated* with no case in the parser refuses the
  request; the unit tests and the fuzz targets are there to find the gap first. Regenerate the docs with
  `UPDATE_DOCS=1 cargo test --test docs`.
- **A protocol.** One adapter in each direction (request builder and reply parser for an upstream; parser
  and renderer for a client), the dispatch arms in `airmate-core/src/adapters.rs`, golden streams in
  `airmate-core/tests/fixtures`, an integration test against the mock upstream in `tests/support`, and a
  live conformance test in `tests/live.rs`.
- **A provider quirk.** A method on `Flavor`, with the date it was documented. It is *verified* only
  once its live test has passed against the real service.

## Tests

`./scripts/build.sh` runs everything CI does. Beyond unit tests:

- golden streams (`UPDATE_GOLDEN=1 cargo test -p airmate-core --test golden` rewrites them; review the diff);
- property tests, including arbitrary-bytes robustness tests;
- integration tests against a scripted mock upstream (`tests/support`), with an injected clock;
- `cargo test --release --test perf -- --ignored --nocapture` for the performance budget;
- `AIRMATE_LIVE=1 cargo test --test live` for live conformance, with the provider variables listed in the
  file (never part of the default run);
- `cd fuzz && cargo +nightly fuzz run <target>` for the fuzz targets.

## Commit messages

Use [conventional commits](https://www.conventionalcommits.org/): `<type>(<scope>): <description>`.
Types are `feat`, `fix`, `docs`, `chore`, `refactor`, `test`, `ci`. Scopes are listed in
[`.omni-dev/scopes.yaml`](.omni-dev/scopes.yaml). CI runs `omni-dev git commit message lint --strict`;
check locally with:

```bash
omni-dev git commit message lint origin/main..HEAD
```

## Pull requests

`main` is protected. Open a pull request; merging goes through CI. Add an entry under
`[Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for any user-visible change.

## Releasing

1. Bump `version` in `Cargo.toml` (and `Cargo.lock`) and move `[Unreleased]` in `CHANGELOG.md` under the new version.
2. Open a pull request titled `chore(release): prepare release vX.Y.Z` and merge it.
3. Tag the merged commit `vX.Y.Z` and push the tag. The `Release` workflow creates the GitHub release,
   attaches binaries for Linux (x86_64, arm64), macOS (arm64) and Windows with checksums, renders a
   Homebrew formula from `packaging/homebrew/airmate.rb.template` and attaches it, pushes the container
   image to the GitHub registry, and publishes `airmate-core` and then `airmate` to crates.io. The Linux
   binaries are checked against a glibc 2.35 floor (`scripts/check-glibc-floor.sh`).

Publishing needs a `CARGO_REGISTRY_TOKEN` repository secret. Both crates share a version.
