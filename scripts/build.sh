#!/bin/bash

# Build script that runs cargo build, fmt check, clippy, tests and docs.
# Exits on first failure. Mirrors what CI checks so problems are caught
# locally rather than only in CI.

set -e

echo "🔨 Building project..."
cargo build

echo "✅ Build successful!"

echo "🎨 Checking code formatting..."
cargo fmt --check

echo "✅ Code formatting check passed!"

echo "🔍 Running clippy checks..."
cargo clippy --all-targets --all-features -- -D warnings

echo "✅ Clippy checks passed!"

echo "🧪 Running tests..."
cargo test

echo "✅ Tests passed!"

# CI denies broken intra-doc links; the plain build does not build rustdoc.
echo "📚 Building docs..."
RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items

echo "✅ All checks passed! 🎉"
