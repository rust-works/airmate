# 5. Failover stops at the first byte

## Context

With several upstreams behind one name, a failed call can be retried elsewhere. Whether that is safe
depends on whether the client has already seen part of the answer. A streamed answer cut off halfway
and continued by another model would splice two different answers together, with the client none the
wiser.

## Decision

- Before the first byte reaches the client, a **retryable** failure (connection error, timeout, 408,
  429, 5xx) moves to the next target; a **non-retryable** one (400, 404, 413, 422) is returned, since
  the next target would say the same. 401 and 403 mean the upstream rejected airmate's key: the
  target's circuit opens at once and the operator is told which setting to check.
- **After the first byte, there is no failover.** A stream that breaks ends with an `error` event, and
  a stream that stops without its terminal event never reports success.
- Per-target **circuit breakers** (consecutive failures or an error rate over a window, a cooldown that
  doubles with jitter on failed probes) keep a dead target from being tried on every request. An
  optional active health check probes upstreams.
- Retries use exponential backoff with full jitter, capped; a `Retry-After` longer than a ceiling marks
  the target cooling down instead of blocking. Attempts and wall-clock time are capped per request.
- Time and randomness are injected, so breakers, backoff and weighted choice are tested
  deterministically against a scripted mock upstream.

## Consequences

A client sometimes sees an error that another target could have answered, but never a spliced answer.
The cost of the guarantee is that a long stream's failure is the client's to retry. Latency before the
first byte includes any failed attempts, bounded by the caps.

## Alternatives considered

- *Buffer the whole answer and fail over until it is complete.* Rejected: it removes streaming.
- *Continue a broken stream on another target.* Rejected: the two models would not agree on what had
  been said.
