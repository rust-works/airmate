#!/bin/bash

# The Linux release binaries link dynamically against glibc, so the newest glibc symbol version
# they reference is the oldest host they run on. This fails when a build raises that floor past the
# one the release is documented to support.
#
# Usage: check-glibc-floor.sh BINARY MAX_VERSION      e.g. check-glibc-floor.sh airmate 2.35
# With BINARY "-" the symbol table is read from stdin (the output of `objdump -T`), for testing.

set -euo pipefail

binary="${1:?usage: check-glibc-floor.sh BINARY MAX_VERSION}"
max="${2:?usage: check-glibc-floor.sh BINARY MAX_VERSION}"

if [ "$binary" = "-" ]; then
    symbols=$(cat)
else
    symbols=$(objdump -T "$binary")
fi

highest=$(echo "$symbols" | grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -V | tail -1 || true)

if [ -z "$highest" ]; then
    echo "no glibc symbols found; is $binary a dynamically linked Linux binary?" >&2
    exit 1
fi

newest=$(printf '%s\n%s\n' "$highest" "$max" | sort -V | tail -1)
if [ "$newest" != "$max" ]; then
    echo "$binary needs glibc $highest, newer than the supported floor $max" >&2
    exit 1
fi

echo "$binary needs glibc $highest (floor $max)."
