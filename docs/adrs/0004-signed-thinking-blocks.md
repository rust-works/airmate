# 4. Reasoning is returned in signed blocks

## Context

Anthropic clients send the model's reasoning back on the next turn, with a `signature` Anthropic
verifies. With a different model behind the gateway there is nothing to verify against, but clients
still send it. Replaying reasoning from one upstream to another is wrong: it makes the second
upstream reason from text it never produced, or rejects the request. Some upstreams also need their own
opaque state back (an OpenAI encrypted reasoning item, a Gemini thought signature, a Bedrock reasoning
signature) for multi-turn tool use to work.

## Decision

airmate signs the reasoning it returns: `am1.<payload>.<mac>`. The payload names the upstream and
protocol that produced the reasoning and may carry that upstream's opaque state, encoded; the MAC is
HMAC-SHA256 over the payload and the reasoning text, keyed from the gateway token. On the next turn a
signature that verifies **and** whose origin is the target being called is replayed as that upstream's
reasoning. Anything else (foreign origin, bad signature, token rotated) is dropped and reported as
ignored.

The MAC is verified in constant time. The key is derived from the token with a domain-separation
string, so the token itself is never used as a MAC key.

## Consequences

Reasoning survives multi-turn conversations with the upstream that produced it and is never leaked to
another. Rotating the token invalidates old signatures, and the next turn proceeds without the
reasoning. The payload is signed, not encrypted: a client can read reasoning it was told was omitted.
Gemini's client protocol does not replay reasoning at all, since its SDKs treat signatures as opaque
bytes and return them only on function calls.

## Alternatives considered

- *Pass signatures through.* Rejected: they would be rejected by, or meaningless to, the next upstream.
- *Always drop reasoning on later turns.* Rejected: it breaks tool use on upstreams that require their
  reasoning state back, and degrades answers on the rest.
- *Encrypt the payload.* Deferred: it hides reasoning from the client, which the protocol does not
  promise, and adds a key-management story for little gain.
