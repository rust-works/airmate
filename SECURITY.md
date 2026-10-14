# Security policy

## Reporting a vulnerability

Please report security problems privately, not in a public issue: use GitHub's
[private vulnerability reporting](https://github.com/rust-works/airmate/security/advisories/new) for
this repository. Include what you did, what you expected and what happened, and the version
(`airmate version`). You will get an acknowledgement, and we will agree a fix and disclosure date with
you. Please do not test against services you do not own.

## Supported versions

Security fixes go to the latest release. airmate is pre-1.0: there are no long-term-support branches.

## What airmate protects, and from whom

airmate is a **credential deputy**: whoever can reach it can spend the operator's upstream keys. Its
surface is default-closed. The threat model, in short:

| Threat | Mitigation |
|---|---|
| A web page reaches the gateway with `fetch` | `Origin` and `Sec-Fetch-Site` refusal; a token is required |
| DNS rebinding | `Host` checked against the bound authority |
| A local process spends the key | the token file is `0600` in a `0700` directory |
| Upstream key exfiltration | never forwarded to a client; scrubbed from errors, logs, metrics and status; no key in `argv` |
| Token in `ps` or shell history | a token is never a command-line flag |
| SSRF through configured URLs | upstream URLs are operator configuration; image URLs are passed to the upstream, never fetched |
| Request smuggling, oversized bodies | a body cap and a strict HTTP parser |
| Thinking-signature forgery | HMAC-SHA256 keyed from the token; constant-time verification |
| Resource exhaustion | concurrency and queue caps, bounded stream buffers, timeouts everywhere |
| A malformed request taking the gateway down | every parser is fuzzed; a parser gap refuses the request rather than panicking |

The full model is in the [operator guide](docs/operator-guide.md#security) and the reasoning in the
[architecture decisions](docs/adrs/0001-default-closed-surface.md).

## Out of scope

- A process running as the same user as the gateway: it can read the token file. That trust boundary is
  stated, not defended.
- The security of the upstream services airmate calls.
- A gateway exposed beyond loopback by an operator who set `insecure_remote`.

## Verification

Tests search the request log, usage ledger, body log, metrics, status output, client responses and
`Debug` output for every credential after runs that exercise success and failure paths (found and fixed
an upstream error echoing its `Authorization` header). The parsers run under property tests on every CI
run and under `cargo fuzz` weekly. `cargo audit`, `cargo deny` and secret scanning run in CI.
