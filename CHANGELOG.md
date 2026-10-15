# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

airmate is now a working gateway; the earlier `Hello, world!` placeholder is gone.

### Added

- **Translation** between Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, Google Gemini
  (client and upstream) and AWS Bedrock Converse (upstream), with streaming, tool use, images,
  structured output and reasoning. Every request field is classed translated, ignored or rejected
  (`airmate fields`), and the classes are documented in `docs/translation.md`.
- **Signed thinking blocks** so reasoning is replayed only to the upstream that produced it.
- **The server**: loopback-only by default, a guard that refuses foreign `Host`, `Origin` and cross-site
  requests before reading a body, tokens and named client keys, streaming with backpressure, keep-alives
  and cancellation, TLS (`server.tls`), a Unix-domain-socket listener, sockets inherited from launchd or
  systemd, and graceful shutdown that ends open streams with an error event.
- **Routing**: several upstreams behind one model name; fallback, weighted, round-robin, least-latency
  and least-cost strategies; rules on the request, including an HTTP classifier; a capability catalog;
  conversation affinity; retries with jittered backoff; circuit breakers; health checks; per-client and
  per-upstream rate and concurrency limits; and failover that stops at the first byte.
- **Clients, budgets and usage**: `airmate keys`, per-client model access, a usage ledger, a price table
  (no prices ship), budgets per day, week and month with an optional fallback model, and `airmate usage`.
- **Observability**: response headers, a request log, an opt-in body log, `/metrics`, `airmate status`,
  `airmate logs`, optional OpenTelemetry traces over OTLP/HTTP, and an admin interface on a Unix socket.
- **Commands**: `serve`, `env`, `run`, `fields`, `route`, `models`, `doctor`, `status`, `reload`,
  `service`, `logs`, `usage`, `keys`, `config`, `completions` and `version`.
- **Embeddings**: `POST /v1/embeddings` to OpenAI-protocol upstreams.
- **Packaging**: release archives with checksums and a glibc floor check, a container image, a Homebrew
  formula, and `airmate-core` published ahead of `airmate`.
- **Quality**: golden streams checked at every byte offset, property and fuzz tests of every parser, a
  mock-upstream conformance suite, a secret-leak test, a performance budget, env-gated live conformance
  tests, and scheduled drift checks against the Anthropic SDK and OpenAI's OpenAPI document.
- The translation core as a separate crate, `airmate-core`, with no I/O.
- Documentation: the operator guide, the translation reference, seven architecture decision records and
  a security policy.

### Changed

- The configuration file is YAML (`airmate.yaml`); the default port is 9997 and `server.port` changes it.

### Known limits

- Upstream `proxy` settings are rejected as not supported yet.
- There is no admin interface on Windows yet.
- Provider quirks are documented, and only verified once their live conformance test has passed against
  the real service; none has been run yet.
- Bedrock Converse cannot do structured output, and Gemini and Bedrock refuse image URLs.
