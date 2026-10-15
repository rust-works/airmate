# Operator guide

How to run airmate: configure it, point your clients at it, route between upstreams, keep it safe
and see what it is doing. For what it does to each request field see
[translation.md](translation.md).

- [Quick start](#quick-start)
- [Configuration](#configuration)
- [Upstream recipes](#upstream-recipes)
- [Model mapping](#model-mapping)
- [Routing cookbook](#routing-cookbook)
- [Failover](#failover)
- [Clients, limits and budgets](#clients-limits-and-budgets)
- [Security](#security)
- [Observability](#observability)
- [Running as a service](#running-as-a-service)
- [Checking an upstream with doctor](#checking-an-upstream-with-doctor)
- [Troubleshooting](#troubleshooting)

Complete example configurations below start with `# validate`: a test loads each one, so they are
known to parse and pass validation. Fragments are not tested.

## Quick start

One upstream needs no file:

```bash
export AIRMATE_UPSTREAM_URL=https://api.fireworks.ai/inference
export AIRMATE_UPSTREAM_PROTOCOL=anthropic          # chat, responses or anthropic
export AIRMATE_FLAVOR=fireworks
export AIRMATE_API_KEY=...                          # or AIRMATE_API_KEY_FILE / AIRMATE_API_KEY_COMMAND
export AIRMATE_MODEL=accounts/fireworks/models/kimi-k2p6
airmate serve                                       # listens on http://127.0.0.1:9997
```

Then point a client at it. `airmate env` prints the settings and `airmate run` sets them for one
command:

```bash
eval "$(airmate env)"              # ANTHROPIC_BASE_URL and ANTHROPIC_AUTH_TOKEN
airmate env --inbound chat         # OPENAI_BASE_URL and OPENAI_API_KEY
airmate env --inbound gemini       # GOOGLE_GEMINI_BASE_URL and GEMINI_API_KEY
airmate env --format json          # an {"env": {...}} object for a client's settings file
airmate run -- claude              # start a private gateway on a free port and run claude through it
```

The gateway token is created on first use and kept in the state directory (`gateway.token`, mode
`0600`), so the settings survive restarts. A token is never accepted as a command-line flag: it would
appear in `ps` and shell history.

## Configuration

### Where it comes from

Highest precedence first: command-line flags, `AIRMATE_*` environment variables, the selected
profile, the config file, built-in defaults. The file is `$AIRMATE_CONFIG` or `--config`, else
`airmate.yaml` in the config directory:

| | Config | State (token, logs, ledger) | Runtime (admin socket) |
|---|---|---|---|
| Linux | `$XDG_CONFIG_HOME/airmate` or `~/.config/airmate` | `$XDG_STATE_HOME/airmate` or `~/.local/state/airmate` | `$XDG_RUNTIME_DIR/airmate`, else `<state>/run` |
| macOS | `~/Library/Application Support/airmate` | `<config>/state` | `<state>/run` |
| Windows | `%APPDATA%\airmate` | `%LOCALAPPDATA%\airmate` | `<state>\run` |

`airmate config path` says where it looks, `airmate config show` prints the effective settings and
where each came from, and `airmate config check` validates the file and resolves its secrets. The
file is strict: an unknown key is an error, with a line number.

The port defaults to **9997**. Set it with `server.port`, `AIRMATE_PORT` or `--port`; `0` asks for
any free port. A taken port is an error, never a silent fallback to another one.

### A complete file

```yaml
# validate
server:
  bind: 127.0.0.1          # loopback by default; see Security before changing it
  port: 9997
  max_concurrent: 16       # upstream calls in flight, across all upstreams
  queue_timeout_secs: 5    # wait this long for a slot instead of answering 429 at once
  unknown_fields: reject   # or warn: ignore and report fields the policy table does not know
  think_tags: true         # split a leading <think> block out as reasoning
  affinity: true           # keep a conversation on the target that served its last turn
  shutdown_timeout_secs: 30

upstreams:
  - name: fireworks
    url: https://api.fireworks.ai/inference
    protocol: anthropic    # chat (default), responses, anthropic, gemini or bedrock
    flavor: fireworks      # generic (default), openai or fireworks
    api_key: { env: FIREWORKS_API_KEY }
    max_concurrent: 8
    timeouts: { connect_secs: 10, first_byte_secs: 60, idle_secs: 120 }
    rate: { rpm: 600, tpm: 2000000 }
    health_check: { path: /v1/models, interval_secs: 30 }
  - name: local
    url: http://127.0.0.1:11434
    protocol: chat

models:
  - match: "claude-opus-*"
    name: big
    strategy: fallback     # fallback, weighted, round_robin, least_latency or least_cost
    targets:
      - { upstream: fireworks, model: accounts/fireworks/models/kimi-k2p6 }
      - { upstream: local, model: qwen3-coder }
  - match: "*"
    targets:
      - { upstream: local, model: qwen3-coder }

clients:
  - name: ci
    key_hash: "sha256:0000000000000000000000000000000000000000000000000000000000000000"
    models: ["claude-haiku-*"]
    rate: { rpm: 120, tpm: 400000 }
    budget: { usd_per_day: 5.0 }

failover: { max_attempts: 4, total_secs: 120, retries: 0 }
circuit: { failure_threshold: 5, cooldown_secs: 30, max_cooldown_secs: 600 }
usage: { ledger: true, prices: builtin }
log: { requests: true, bodies: false, rotate: { max_size_mb: 64, keep: 5 } }
```

### Secrets

A configuration refers to a secret, never contains one. Each secret is a one-key map:

```yaml
api_key: { env: FIREWORKS_API_KEY }                # an environment variable
api_key: { file: /run/secrets/fireworks }          # a file; refused unless owner-only (0600)
api_key: { command: [pass, show, fireworks] }      # a helper whose stdout is the key; no shell, 10 s timeout
api_key: { keychain: airmate-fireworks }           # macOS `security`, Linux `secret-tool`
api_key: { value: sk-... }                         # accepted only with a loud warning
```

Store a keychain entry with `security add-generic-password -s airmate-fireworks -a "$USER" -w` on
macOS or `secret-tool store --label=airmate-fireworks service airmate-fireworks` on Linux. A secret
is held in a type that never prints, and is zeroed on drop.

### Profiles

A profile is a set of overrides applied with `--profile NAME` or `AIRMATE_PROFILE`:

```yaml
# validate
upstreams:
  - { name: local, url: "http://127.0.0.1:11434" }
models:
  - { match: "*", targets: [ { upstream: local, model: qwen3-coder } ] }
profiles:
  debug:
    log: { bodies: true }
    server: { unknown_fields: warn }
```

### Reloading

`airmate reload` (or `SIGHUP`) re-reads the file, validates it and swaps in the upstreams, model
table, rules and client keys atomically. In-flight requests finish on the old tables. A file that
fails validation is rejected and the old one stays in force, with the error reported to you. The
listen address, port, TLS material, concurrency limit and shutdown timeout need a restart; reload
says so when they changed.

## Upstream recipes

Every recipe is a complete file. Replace the model names with ones the service offers.

### Ollama, LM Studio, vLLM and other OpenAI-compatible servers

```yaml
# validate
upstreams:
  - name: local
    url: http://127.0.0.1:11434      # Ollama; LM Studio is :1234, vLLM is :8000
    protocol: chat
models:
  - match: "*"
    targets: [ { upstream: local, model: qwen3-coder } ]
```

Local servers usually need no key. The default `generic` flavor sends `max_tokens`, asks for stream
usage with `stream_options`, and splits `<think>…</think>` text out as reasoning. Set the context
window in `catalog` if the server's is smaller than the model's.

### OpenRouter

```yaml
# validate
upstreams:
  - name: openrouter
    url: https://openrouter.ai/api
    protocol: chat
    api_key: { env: OPENROUTER_API_KEY }
models:
  - match: "*"
    targets: [ { upstream: openrouter, model: "anthropic/claude-sonnet-4.5" } ]
```

### OpenAI (Chat and Responses)

```yaml
# validate
upstreams:
  - name: openai
    url: https://api.openai.com
    protocol: chat              # or responses
    flavor: openai
    api_key: { env: OPENAI_API_KEY }
models:
  - match: "*"
    targets: [ { upstream: openai, model: gpt-4.1 } ]
```

GPT-5 and the o-series ignore `temperature` and `top_p`; airmate drops them and reports them in
`x-airmate-ignored`. Use `protocol: responses` for models that are served only there.

### Fireworks

```yaml
# validate
upstreams:
  - name: fireworks
    url: https://api.fireworks.ai/inference
    protocol: anthropic         # its Anthropic-compatible surface; `chat` also works
    flavor: fireworks
    api_key: { env: FIREWORKS_API_KEY }
models:
  - match: "*"
    targets: [ { upstream: fireworks, model: "accounts/fireworks/models/kimi-k2p6" } ]
```

With `protocol: chat` and `flavor: fireworks`, airmate sends `thinking` or `reasoning_effort`, never
both, which Fireworks rejects. Without the flavor you get that rejection (see
[troubleshooting](#a-400-about-thinking-and-reasoning_effort)).

### Anthropic

```yaml
# validate
upstreams:
  - name: anthropic
    url: https://api.anthropic.com
    protocol: anthropic
    api_key: { env: ANTHROPIC_API_KEY }
models:
  - match: "*"
    targets: [ { upstream: anthropic, model: claude-sonnet-4-5 } ]
```

An Anthropic client reaching an Anthropic upstream is passed through. The client's `anthropic-version`
and `anthropic-beta` headers are forwarded.

### Google Gemini

```yaml
# validate
upstreams:
  - name: gemini
    url: https://generativelanguage.googleapis.com
    protocol: gemini
    api_key: { env: GEMINI_API_KEY }
models:
  - match: "*"
    targets: [ { upstream: gemini, model: gemini-2.5-flash } ]
```

Gemini needs image bytes, so an image sent by URL is refused. Thought signatures are kept and
returned on the first function call; Gemini 3 calls without a recorded one get the documented
placeholder.

### AWS Bedrock

```yaml
# validate
upstreams:
  - name: bedrock
    url: https://bedrock-runtime.us-east-1.amazonaws.com
    protocol: bedrock
    aws:
      region: us-east-1          # read from the URL when it is a bedrock-runtime URL
      access_key_id: { env: AWS_ACCESS_KEY_ID }
      secret_access_key: { env: AWS_SECRET_ACCESS_KEY }
      session_token: { env: AWS_SESSION_TOKEN }     # optional, for temporary credentials
models:
  - match: "*"
    targets: [ { upstream: bedrock, model: "anthropic.claude-3-5-sonnet-20241022-v2:0" } ]
```

Requests are signed with Signature Version 4. A Bedrock API key goes in `api_key` instead of `aws`.
Structured output is refused (Converse has none airmate can use), and so are image URLs.

## Model mapping

A client names a model; airmate resolves the name in this order:

1. an **exact** `match` in the `models` list;
2. the first matching **glob**, in file order;
3. a **direct upstream model name**: if the name is not in the client protocol's own family
   (`claude-*` for Anthropic clients, `gpt-*` and the o-series for OpenAI clients, `gemini-*` for
   Gemini clients) it is sent as-is to the default upstream (the one named `default`, else the first);
4. the default entry `match: "*"`.

So `claude-opus-4-1` maps through your table, while a client that asks for
`accounts/fireworks/models/kimi-k2p6` by name reaches it directly. `airmate models` prints the table
with each target's capabilities, and flags targets the built-in catalog has no entry for (`--check`
exits 3 if any). `airmate route request.json` shows which targets a captured request would use, and
`--explain` shows the steps: the rule that matched, the entry, what was filtered out and why, and the
final order.

Capabilities come from a shipped catalog (context windows, tools, vision, thinking, structured
output) with its `as_of` date. The values are indicative; override them for your models:

```yaml
# validate
upstreams:
  - { name: local, url: "http://127.0.0.1:11434" }
models:
  - { match: "*", targets: [ { upstream: local, model: my-finetune } ] }
catalog:
  - match: my-finetune
    context_window: 32768
    max_output_tokens: 4096
    tools: true
    vision: false
```

A request a target cannot serve (tools, images, thinking, structured output, or more context than its
window) skips that target, and the `x-airmate-skipped` header says why.

## Routing cookbook

### Fallback chain

The default strategy tries targets in order and moves on when one fails:

```yaml
# validate
upstreams:
  - { name: primary, url: "https://api.fireworks.ai/inference", protocol: anthropic, flavor: fireworks, api_key: { env: FW } }
  - { name: backup, url: "https://api.openai.com", flavor: openai, api_key: { env: OA } }
models:
  - match: "*"
    targets:
      - { upstream: primary, model: "accounts/fireworks/models/kimi-k2p6" }
      - { upstream: backup, model: gpt-4.1 }
```

### Spread load, or pick the cheapest or fastest

```yaml
# validate
upstreams:
  - { name: a, url: "http://a.internal:8000" }
  - { name: b, url: "http://b.internal:8000" }
models:
  - match: "*"
    strategy: weighted            # round_robin, least_latency (lowest time to first byte) or least_cost
    targets:
      - { upstream: a, model: m, weight: 3 }
      - { upstream: b, model: m, weight: 1, price: { input_per_mtok: 0.2, output_per_mtok: 0.6 } }
```

`least_cost` orders by price; a target with no price sorts last. A conversation stays on the target
that served its previous turn (`server.affinity`) so provider-side caches stay warm.

### Rules

A rule sends matching requests somewhere else, first match wins. Predicates are ANDed; every field
is optional:

```yaml
# validate
upstreams:
  - { name: big, url: "https://api.anthropic.com", protocol: anthropic, api_key: { env: A } }
  - { name: cheap, url: "http://127.0.0.1:11434" }
models:
  - { match: "*", name: default, targets: [ { upstream: big, model: claude-sonnet-4-5 } ] }
  - { match: "none", name: small, targets: [ { upstream: cheap, model: qwen3-coder } ] }
rules:
  - name: short-prompts-go-small
    when: { est_input_tokens_max: 2000, tools: false, thinking: false }
    use: small
  - name: night-batch
    when: { client: batch, hour_of_day: { from: 22, to: 6 } }
    targets:
      - { upstream: cheap, model: qwen3-coder }
```

Predicates: `model` (glob), `client`, `inbound` (`anthropic`, `chat`, `responses`, `gemini`), `stream`,
`tools`, `images`, `thinking`, `effort_min`, `effort_max`, `est_input_tokens_min/max`,
`max_tokens_min/max`, `header` (only headers listed in `server.rule_headers` are visible),
`hour_of_day` (UTC, wrapping past midnight) and `classifier`.

### A classifier

A classifier is an HTTP endpoint that labels a request, for example by difficulty. It receives
`{"input": {...}}` containing only the fields you allow, never the upstream key, and answers
`{"label": "hard"}`. It has a short timeout and fails open: no label means the rule does not match.

```yaml
# validate
upstreams:
  - { name: big, url: "https://api.anthropic.com", protocol: anthropic, api_key: { env: A } }
  - { name: cheap, url: "http://127.0.0.1:11434" }
models:
  - { match: "*", name: default, targets: [ { upstream: cheap, model: qwen3-coder } ] }
  - { match: "none", name: hard, targets: [ { upstream: big, model: claude-sonnet-4-5 } ] }
classifiers:
  - name: difficulty
    url: http://127.0.0.1:7000/classify
    timeout_ms: 300
    input: [ last_user_message, summary ]     # also: system
rules:
  - name: hard-ones-go-big
    when: { classifier: { name: difficulty, label: hard } }
    use: hard
```

Every classifier call is in the request log (`airmate logs --attempts`).

## Failover

- A **retryable** failure (connection error, timeout, 408, 429, 5xx) moves to the next target; a
  **non-retryable** one (400, 404, 413, 422) does not, since the next target would say the same.
  A 401 or 403 is the upstream rejecting your key: the target's circuit opens at once and the gateway
  logs which setting to check.
- `failover.max_attempts` and `failover.total_secs` cap the work; `failover.retries` retries the same
  target first, with exponential backoff and full jitter between `backoff_base_ms` and
  `backoff_cap_ms`. A `Retry-After` longer than `retry_after_ceiling_secs` marks the target cooling
  down and moves on instead of waiting.
- **No failover after the first byte.** Once any of the answer has been sent to the client, a failure
  ends the stream with an `error` event; retrying elsewhere would splice two answers together.
- A **circuit breaker** per target opens after `failure_threshold` consecutive failures or when the
  error rate over the last `window` outcomes passes `error_rate`; it half-opens after a cooldown that
  doubles (with jitter) on each failed probe up to `max_cooldown_secs`. An optional `health_check`
  probes an upstream actively.
- Every response carries `x-airmate-attempts`, `x-airmate-upstream` and, when targets were passed
  over, `x-airmate-skipped`.

## Clients, limits and budgets

By default one token (the gateway token) is accepted. Named **clients** have their own keys, which
are stored as SHA-256 hashes only:

```bash
airmate keys add ci --models 'claude-haiku-*' --rpm 120 --tpm 400000 --budget-per-day 5 --save
airmate keys list
airmate keys rotate ci
airmate keys revoke ci
```

The key is shown once. `--save` also keeps it, owner-only, so `airmate env --client ci` and
`airmate run --client ci -- …` can use it. Keys made this way live in a managed `clients.yaml` next to
the main file, and a running gateway is reloaded so a revoked key stops working at once. A client
defined in your own `airmate.yaml` is listed by `keys list` but changed by editing that file.

The client name goes on every log record, metric label and rule predicate.

- **Rate limits** (`rate: { rpm, tpm }`) are token buckets per client and per upstream; the token
  bucket is charged an estimate before the call and settled from real usage after. A client over its
  limit gets `429` with `Retry-After`; an upstream over its limit is skipped and the next target tried.
- **Concurrency** is limited globally (`server.max_concurrent`, with an optional wait,
  `server.queue_timeout_secs`) and per upstream (`max_concurrent`).
- **Budgets** (`budget: { usd_per_day, usd_per_week, usd_per_month }`, UTC periods) are spend caps
  computed from the usage ledger and the price table. Past a cap the answer is
  `429 budget exhausted` with a `Retry-After` to the next period, never a silent downgrade, unless the
  client names `budget_fallback: <model entry>`, which then serves the request. A request whose model
  has no price has unknown cost and counts for nothing: **no prices ship with airmate**, so set
  `price` on a target, in `catalog`, or in a price file:

```yaml
# a price file, named by `usage.prices: /path/to/prices.yaml`
prices:
  - { match: "gpt-4.1*", input_per_mtok: 2.0, output_per_mtok: 8.0, cache_read_per_mtok: 0.5 }
```

`airmate usage [--by client|model|upstream|day] [--since 7d|2026-10-01] [--format table|json]`
aggregates the ledger (`usage.ledger: true`). Without a ledger, budgets still hold but start from zero
after a restart.

## Security

airmate is a credential deputy: whoever can reach it can spend your upstream keys. Its surface is
default-closed.

- **Listener.** It binds `127.0.0.1` only. A non-loopback `bind` needs an `allowed_hosts` list **and**
  either `server.tls` or `insecure_remote: true` (`--insecure-remote`) acknowledging the risk.
  `server.unix_socket` listens on a Unix-domain socket (mode `0600`) instead.
- **The guard** runs before any body is read: a `Host` that is not loopback on the bound port or in
  `allowed_hosts` is refused (this defeats DNS rebinding); any `Origin` header or a
  `Sec-Fetch-Site` other than `none` is refused (a web page you visit cannot reach it); then a valid
  token is required. `HEAD /` and other unauthenticated probes get `401` and learn nothing.
- **Tokens** are 32 bytes from the OS CSPRNG, accepted as `x-api-key`, `x-goog-api-key` or
  `Authorization: Bearer`, and compared in constant time. The state directory is `0700`; airmate
  refuses to start if the token file is group- or world-readable.
- **Upstream keys** never reach the client, and the gateway token never reaches the upstream. Any
  upstream error text is scrubbed of both before it is returned, logged or shown by `status`.
- **TLS.** Serve HTTPS with
  `server: { tls: { cert: /path/cert.pem, key: /path/key.pem } }` (the default `tls` feature).
- **Admin.** `status`, `reload` and `/metrics` are on a Unix-domain socket in the runtime directory
  (`0600`), not on the data-plane port, so a data-plane token cannot administer the gateway. There is
  no admin interface on Windows yet.
- **Logs.** Request and response bodies are logged only with `log.bodies: true`, to their own file with
  a size cap and a retention period; thinking text and tool inputs are bodies. Headers are never
  logged.

Threat model:

| Threat | Mitigation |
|---|---|
| A web page reaches the gateway with `fetch` | `Origin` and `Sec-Fetch-Site` refusal; a token is required |
| DNS rebinding | `Host` checked against the bound authority |
| A local process spends the key | the token file is `0600`; the same-user trust boundary is stated, not hidden |
| Upstream key exfiltration | never forwarded; redacted everywhere; no key in `argv` |
| Token in `ps` or history | never a flag |
| SSRF through configured URLs | upstream URLs are operator configuration; image URLs are passed *to* the upstream, never fetched |
| Request smuggling, oversized bodies | `server.max_body_bytes`; a strict HTTP parser |
| Signature forgery | HMAC-SHA256 keyed from the token; constant-time verification |
| Resource exhaustion | concurrency and queue caps, bounded stream buffers, timeouts everywhere |

## Observability

### Response headers

| Header | Meaning |
|---|---|
| `x-airmate-upstream`, `x-airmate-upstream-model` | the target that answered |
| `x-airmate-attempts` | attempts made |
| `x-airmate-rule` | the rule that selected the entry, if any |
| `x-airmate-skipped` | targets passed over and why |
| `x-airmate-ignored` | request parts ignored by the field policy |
| `x-airmate-token-estimate: heuristic` | on `count_tokens` |
| `request-id` | per-request id, also in error bodies; echoed from `x-request-id` when the client sends one |

### Logs and the ledger

`requests.jsonl` has one record per request and one per upstream attempt (timestamps, ids, client,
protocols, model asked, rule, target, status, error class, times, token counts, estimated cost,
ignored and rejected fields). `airmate logs [--follow] [--client X] [--status 5xx] [--since 1h]
[--attempts] [--format oneline|json]` searches it. `usage.jsonl` is the ledger.

### Status

`airmate status` shows the version, uptime, listen address, a config hash, each upstream's health and
each target's circuit state and latency, requests in flight, recent errors and a tally of the fields
clients sent that were ignored. It exits 3 when the gateway is down or something is unhealthy.

### Metrics

`GET /metrics` on the admin socket, in Prometheus text format:

| Metric | Labels |
|---|---|
| `airmate_requests_total` | `inbound`, `client`, `target`, `status` |
| `airmate_request_duration_seconds` (histogram) | `inbound` |
| `airmate_ttfb_seconds` (histogram) | `target` |
| `airmate_attempts_total` | `target`, `outcome` |
| `airmate_failovers_total` | `from`, `to` |
| `airmate_circuit_state` (0 closed, 1 half-open, 2 open) | `target` |
| `airmate_inflight` | `target` |
| `airmate_tokens_total` | `target`, `kind` |
| `airmate_cost_usd_total` | `client`, `target` |
| `airmate_ignored_fields_total` | `field` |
| `airmate_policy_rejections_total` | `field` |

Label cardinality is capped, because some labels come from request content.

### Traces

With an `otel` block the gateway exports one span per request as OTLP over HTTP with JSON bodies:

```yaml
otel:
  endpoint: http://127.0.0.1:4318
  service_name: airmate
  headers: { x-vendor-key: { env: VENDOR_KEY } }
```

Export is best effort: spans are dropped when the collector is behind, and a dead collector is
reported once and never slows a request.

## Running as a service

`airmate serve` is a foreground process, fine for a terminal, a container or any supervisor.

```bash
airmate service install      # launchd agent (macOS) or systemd user units (Linux) or a logon task (Windows)
airmate service status
airmate service logs -f
airmate service uninstall
```

On macOS and Linux the service manager owns the listening socket, so the gateway starts on the first
connection and at login and keeps the socket across restarts. `--dry-run` prints the files and
commands without running them; `--manager detached` runs a plain background process with its output in
the state directory. A `--config` or `--profile` you give is written into the service's command line.

On `SIGTERM` or `SIGINT` the gateway stops accepting, lets requests in flight finish for up to
`server.shutdown_timeout_secs`, and ends any stream still open with an `error` event. It raises the
soft open-file limit toward 4096 and backs off on `EMFILE` instead of spinning.

A container image and a Homebrew formula are attached to each release; see the repository's
`Dockerfile`. Publish the container's port on loopback only (`-p 127.0.0.1:9997:9997`).

## Checking an upstream with doctor

```bash
airmate doctor                        # every upstream, through the same adapters the gateway uses
airmate doctor --upstream fireworks --model accounts/fireworks/models/kimi-k2p6
```

It sends a plain call, a streamed call and, for models that take tools, a call that must use a tool,
plain and streamed, and reports what worked. It exits 3 if any probe fails. The same probes back the
live conformance tests, which run only when asked for:

```bash
AIRMATE_LIVE=1 AIRMATE_LIVE_OPENAI_KEY=... cargo test --test live -- --nocapture
```

`tests/live.rs` lists the variables for each provider. A provider quirk in
[translation.md](translation.md#provider-flavors) is *verified* only once its live test has passed.

## Troubleshooting

### `401 authentication_error`

The client's token does not match. Run `airmate env` again (the token is in the state directory), or
check that the client sends it as `x-api-key`, `x-goog-api-key` or a bearer token. A named client uses
its own key, not the gateway token.

### `403 permission_error` naming an upstream key

The *upstream* rejected the key airmate sent. The message names the setting
(`upstreams[fireworks].api_key`). Check the variable or file it points at; `airmate config check`
resolves the secrets and says which one is missing. A rejected key opens that target's circuit so it
is not hammered, and `airmate status` shows it.

### `403` about the `Host` or `Origin`

The guard refused the request before reading it. A request through a hostname other than loopback
needs `server.allowed_hosts`; a request from a web page is never accepted.

### `400` saying a field is "not known to airmate"

The client is newer than the field policy table. Set `unknown_fields: warn` (or
`AIRMATE_UNKNOWN_FIELDS=warn`) to ignore and report unknown fields, and report the field. The weekly
drift check normally catches new ones first.

### `400` saying a field is "not supported by airmate"

The field is *rejected* on purpose (server tools, the MCP connector, containers, cached content …):
airmate cannot honour it and will not pretend to. The message says why; `airmate fields --inbound
<protocol>` lists the table.

### `404` "not mapped to an upstream model"

No `models` entry matched the name and there is no `match: "*"` entry or default upstream. Add one;
`airmate models` shows the table and `airmate route request.json --explain` shows the resolution.

### `429 budget exhausted`

The client reached its `budget`. The message names the period and when it renews. Raise the budget,
name a `budget_fallback`, or wait. `airmate usage --by client` shows the spend.

### `529 overloaded_error` listing skipped targets

Every target was skipped or failed. The message and `x-airmate-skipped` say why each one was:
`circuit-open`, `unhealthy`, `rate-limit`, `concurrency`, `cooling-down` or a capability it lacks.

### A 400 about `thinking` and `reasoning_effort`

Fireworks on the Chat protocol rejects a request that sets both. Use `flavor: fireworks`, which sends
one of them.

### "What actually went upstream?"

`airmate logs --attempts` shows each attempt. For the bodies, turn on `log.bodies` (or use the `debug`
profile above), reproduce the request, and read `bodies.jsonl` in the state directory; turn it off
again and delete the file when done, since it holds prompts and completions.

### The port is taken

airmate never falls back to another port. Stop what is using 9997, choose another with `--port`, or
use `--port 0` for any free one. If `airmate service install` was used, `airmate service status`
shows whether it is already running.

### `airmate status` says it is not running but `serve` is

The admin socket is in the runtime directory. A different `HOME`, `XDG_RUNTIME_DIR` or `XDG_STATE_HOME`
in the two shells changes where it is. `airmate config path` and the "no admin socket" warning at
startup say where.

## Known limits

- `count_tokens` is an estimate; near a context limit it can be off by a margin that matters.
- `stop_sequence` is never reported; a stop on one reads as `end_turn`.
- Server tools, Anthropic-defined client tools (`bash_*`, `text_editor_*`; define a custom tool
  instead), the MCP connector, containers, citations and PDF documents on most upstreams are not
  supported.
- Prompt caching is as good as the upstream's own; cache writes are not reported by any
  OpenAI-protocol upstream.
- Signed reasoning stops verifying when the token is rotated; the next turn proceeds without it. The
  signature is signed, not encrypted.
- Reasoning from earlier turns is not replayed on the Gemini client protocol.
- An upstream `proxy` is not supported yet (the setting is rejected).
- There is no admin interface on Windows yet, so `status`, `reload` and `/metrics` need Unix-domain
  sockets.
- Provider quirks are as documented on the stated date, and only as verified as their live tests.
