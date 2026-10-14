#!/bin/bash

# airmate-core is sans-I/O: no async runtime, HTTP stack, TLS, clock or randomness crates may
# appear anywhere in its normal dependency tree. Exits non-zero naming the first offender.

set -euo pipefail

cd "$(dirname "$0")/.."

banned='^(tokio|mio|socket2|hyper|hyper-util|h2|http-body-util|reqwest|rustls|ring|aws-lc-rs|openssl|native-tls|rand|getrandom|chrono|time)$'

deps=$(cargo tree -p airmate-core --edges normal --prefix none --format '{p}' | awk '{print $1}' | sort -u)

offenders=$(echo "$deps" | grep -E "$banned" || true)
if [ -n "$offenders" ]; then
    echo "airmate-core depends on I/O crates:" >&2
    echo "$offenders" >&2
    exit 1
fi

echo "airmate-core has no I/O dependencies."
