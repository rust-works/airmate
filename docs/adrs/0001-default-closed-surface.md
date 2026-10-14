# 1. The surface is default-closed

## Context

airmate holds credentials for paid upstream services and answers anyone who can reach its port. A
local gateway is a tempting target for three kinds of caller that are not the operator: a web page the
operator visits (which can make requests to `localhost`), another process on the machine, and anyone
on the network if the bind address is widened. Convenience pulls the other way: a gateway nobody can
start in one command does not get used.

## Decision

Everything is closed until the operator opens it, and opening is explicit.

- It binds `127.0.0.1` only, and a taken port is an error, never a fallback to another one.
- A non-loopback bind requires an `allowed_hosts` list **and** either TLS or `insecure_remote`
  acknowledging the risk.
- Every request passes one guard before any body is read: `Host` must be loopback on the bound port or
  listed, `Origin` and cross-site `Sec-Fetch-Site` are refused, and a token is required. Unauthenticated
  probes learn nothing.
- Tokens are random, compared in constant time, stored `0600` in a `0700` directory, and never accepted
  on the command line. Named client keys are stored as hashes.
- Upstream keys are never forwarded to the client and the gateway token never to the upstream; both are
  scrubbed from anything that is returned, logged or shown.
- Bodies are not logged unless asked, and then to a separate file with a cap and a retention period.

The first run still takes one command: the token is created on first use and `airmate env` prints what
a client needs.

## Consequences

Setting up a remote or shared gateway takes deliberate steps, and each is one the operator can see in
the configuration. A request from a browser extension or a script that sets `Origin` is refused even
with a valid token. The same-user trust boundary on a single machine is stated rather than papered
over: a process running as the operator can read the token file.

## Alternatives considered

- *Bind to all interfaces with a token.* Rejected: one leaked token or a DNS-rebinding page would
  spend the operator's keys, and the default is what most people keep.
- *A generated token printed at startup only.* Rejected: client settings would break on every restart.
- *Fall back to another port when 9997 is taken.* Rejected: clients configured for 9997 would silently
  talk to something else, or to nothing.
