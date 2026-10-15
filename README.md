# airmate

A local, default-closed router for AI requests. Point a client that speaks the Anthropic, OpenAI or
Gemini API at airmate, and it translates the request, picks an upstream model, calls it and
translates the answer back, streaming included. Claude Code, the OpenAI SDKs and the Gemini CLI can
all use models they were never built for.

```text
client (Anthropic / Chat / Responses / Gemini)  ->  airmate  ->  Anthropic, OpenAI Chat or Responses,
                                                                  Gemini, Bedrock, Ollama, vLLM,
                                                                  LM Studio, OpenRouter, Fireworks, ...
```

## Quick start

```bash
cargo install airmate

# One upstream needs no config file:
export AIRMATE_UPSTREAM_URL=https://api.fireworks.ai/inference
export AIRMATE_UPSTREAM_PROTOCOL=anthropic
export AIRMATE_FLAVOR=fireworks
export AIRMATE_API_KEY=...
export AIRMATE_MODEL=accounts/fireworks/models/kimi-k2p6
airmate serve &

airmate run -- claude        # Claude Code, pointed at the gateway
```

`airmate env` prints the settings for other clients (`--inbound chat` for the OpenAI SDKs,
`--inbound gemini` for Gemini). The gateway listens on `127.0.0.1:9997`; change it with `server.port`,
`AIRMATE_PORT` or `--port`.

## What it does

- **Translates** between Anthropic Messages, OpenAI Chat Completions, OpenAI Responses, Google Gemini
  and AWS Bedrock Converse, including streaming, tool use, images, structured output and reasoning.
  Every request field is classed *translated*, *ignored* (and reported) or *rejected*: nothing is
  silently dropped. See [docs/translation.md](docs/translation.md).
- **Routes.** Several upstreams behind one model name, with fallback, weighted, round-robin,
  least-latency and least-cost strategies, rules on the request (size, tools, images, client, hour,
  an external classifier), conversation affinity, retries with jittered backoff, circuit breakers and
  health checks. A request never fails over after the first byte has been sent.
- **Limits and budgets.** Named client keys, per-client model access and rate limits, concurrency
  limits, and per-day, week and month spend budgets from a usage ledger.
- **Is safe by default.** Loopback only, a token required, browser requests and DNS rebinding refused,
  secrets never in the configuration or the logs, administration on a Unix socket.
- **Shows its work.** Response headers, a request log, a usage ledger, `/metrics`, `airmate status`,
  optional OpenTelemetry traces, and `airmate doctor` to test an upstream.
- **Runs as a service** (`airmate service install`: launchd, systemd, a logon task or a detached
  process), over TLS if you ask, with graceful shutdown.

## Supported clients and upstreams

| | Client | Upstream |
|---|---|---|
| Anthropic Messages | Claude Code, Anthropic SDKs | Anthropic, Fireworks, any Anthropic-compatible server |
| OpenAI Chat Completions | OpenAI SDKs and most tools | OpenAI, Ollama, LM Studio, vLLM, OpenRouter, Fireworks, ... |
| OpenAI Responses | Codex and the Responses SDK | OpenAI |
| OpenAI embeddings | any | OpenAI-protocol upstreams |
| Google Gemini | Gemini SDKs and CLI | Gemini |
| AWS Bedrock Converse | - | Bedrock (SigV4 or a Bedrock API key) |

## Documentation

- [Operator guide](docs/operator-guide.md): configuration, upstream recipes, routing, security,
  observability, running as a service, troubleshooting.
- [Translation reference](docs/translation.md): the rules, and the generated field policy tables.
- [Architecture decisions](docs/adrs/README.md).
- [Security policy](SECURITY.md), [contributing](CONTRIBUTING.md), [changelog](CHANGELOG.md).

## Limitations

- `count_tokens` is an estimate.
- `stop_sequence` is never reported; a stop on one reads as `end_turn`.
- Server tools, Anthropic-defined client tools, the MCP connector, containers and citations are not
  supported, and are rejected by name rather than ignored.
- Prompt caching is as good as the upstream's own.
- Signed reasoning stops verifying when the token is rotated.
- Provider quirks are as documented on the stated date and only as verified as their live conformance
  tests, which need real keys and are not part of the default test run.
- There is no admin interface on Windows yet.

## Development

```bash
./scripts/build.sh   # build, fmt check, clippy, tests, docs: what CI checks
```

The translation core is a separate crate, [`airmate-core`](airmate-core), with no I/O. See
[CONTRIBUTING.md](CONTRIBUTING.md).

## License

[BSD 3-Clause](LICENSE)
