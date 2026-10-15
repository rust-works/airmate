# 6. YAML configuration that refers to secrets and never holds them

## Context

The configuration describes upstreams, models, rules, clients and limits, and ends up in dotfiles
repositories, chat messages and bug reports. The things it must not contain are the credentials it
configures.

## Decision

- The file is **YAML** (`airmate.yaml`), strict: an unknown key is an error with a line number. The
  default port is 9997 and `server.port` changes it.
- Precedence, highest first: flags, `AIRMATE_*` environment variables, the selected profile, the file,
  defaults. One upstream needs no file at all (`AIRMATE_UPSTREAM_URL` and friends).
- A secret is a **one-key map** naming where it lives: `env`, `file` (which must be owner-only),
  `command` (run without a shell, with a timeout), `keychain` (the platform's own tool), or `value`,
  accepted only with a loud warning. In memory a secret is a type that redacts on `Debug` and `Display`
  and zeroes on drop.
- Client keys created by `airmate keys` go in a managed `clients.yaml`, as hashes only, so the
  operator's own file is never rewritten.
- A reload validates everything, then swaps atomically; a bad file is rejected with the old one still
  in force. Settings that need a new socket (bind, port, TLS) need a restart and say so.

## Consequences

A config can be shared and committed. Rotating a key changes the environment or a file, not the
configuration. Tests grep the request log, ledger, body log, metrics, status, client responses and
`Debug` output for every secret after a run that exercises success and failure paths, and that test
found a real leak (an upstream error echoing its `Authorization` header), now scrubbed.

## Alternatives considered

- *TOML.* Preferred by the specification's author for strictness; YAML was chosen by the project
  owner for readability of the nested model and rule tables. Strictness is recovered with
  unknown-key errors.
- *Secrets inline.* Rejected as the primary route; `value` exists for throwaway setups and warns.
