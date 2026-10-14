# Contributing

## Before you push

```bash
./scripts/build.sh
```

This runs `cargo build`, `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`,
`cargo test` and `cargo doc` with warnings denied, which are the same checks CI runs.

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
   attaches binaries for Linux (x86_64, arm64), macOS (arm64) and Windows, and publishes to crates.io.

Publishing needs a `CARGO_REGISTRY_TOKEN` repository secret.
