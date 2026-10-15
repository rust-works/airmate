# 7. Administration is on a Unix-domain socket, not the data-plane port

## Context

`status`, `reload` and `/metrics` expose how the gateway is configured and what it is doing, and
`reload` changes its behaviour. If they lived on the data-plane port, anything holding a data-plane
token (every client, every tool configured with `airmate env`) could administer the gateway.

## Decision

The admin interface is a small HTTP server on a Unix-domain socket in the runtime directory: mode
`0600` in a `0700` directory, with a stale socket from a dead process replaced and a live one refused.
The data plane has no admin routes at all. `airmate status`, `airmate reload` and `airmate keys` (which
reloads a running gateway so a revoked key stops working at once) talk to it. `/metrics` is Prometheus
text, hand-rolled, with the label cardinality of request-derived labels capped.

## Consequences

Administration is gated by filesystem permissions, which are what the rest of the design already relies
on. There is no admin interface on Windows yet; the specification's plan is a loopback port protected by
a separate admin token, and until that exists `status` and `reload` report that they need Unix sockets.
Scraping metrics needs a tool that can read a Unix socket, or a sidecar that proxies it.

## Alternatives considered

- *A second TCP port on loopback.* Rejected as the default: any local process, and any web page that can
  reach loopback, could try it; it is the fallback plan for Windows, with a separate token.
- *Signals only.* Rejected: `SIGHUP` cannot report a rejected configuration back to the person who sent
  it, or return status.
