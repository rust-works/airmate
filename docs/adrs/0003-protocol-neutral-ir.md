# 3. One protocol-neutral intermediate representation

## Context

Supporting N client protocols and M upstream protocols directly means N×M translators, each with its
own bugs and its own idea of what a tool call is. Streaming makes it worse: events are emitted in each
protocol's own order and granularity.

## Decision

Each protocol has an adapter to and from a single IR: turns made of text, images, documents, tool calls,
tool results and reasoning; tools with JSON Schema inputs; sampling, limits, stop sequences and
reasoning controls. A client protocol has a parser into the IR and a renderer out of it; an upstream
protocol has a request builder out of it and a reply parser into it. Streaming is the same shape: a
parser emits IR events, and a renderer turns them into the client's events. A *sequencer* between them
turns possibly interleaved events (parallel tool calls streamed together) into strictly sequential
output within a buffer cap.

The IR is Anthropic-shaped where the protocols differ (usage excludes cached tokens; reasoning is a
distinct part), because it is the richest of the three on content. Same-protocol pairs bypass it: the
body is forwarded with the model mapped and unsupported fields stripped and reported.

The translation core is its own crate, `airmate-core`, with no I/O: no sockets, clock, randomness or
filesystem. A CI job checks its dependency tree contains none of the crates that would bring them.

## Consequences

A new protocol is one adapter in each direction, not a translator per existing protocol. The core is
testable without a network: golden streams checked at every byte offset, property tests on the
sequencer, fuzz targets. Round trips through the IR can lose what the IR has no place for, which is
what the field policy exists to make visible.

## Alternatives considered

- *Pairwise translators.* Rejected: quadratic growth and inconsistent behaviour between pairs.
- *Anthropic's wire format as the pivot.* Rejected: it would drag its field names and quirks into every
  adapter and make "Anthropic to Anthropic" indistinguishable from a translation.
