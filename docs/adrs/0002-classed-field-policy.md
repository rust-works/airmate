# 2. Every field is classed: translated, ignored or rejected

## Context

The protocols airmate translates between do not line up. Anthropic has server tools, citations and
`cache_control`; OpenAI has `logit_bias` and `store`; Gemini has `safetySettings`. A translator that
quietly drops what the target lacks produces answers that differ from what the client asked for and
gives it no way to know.

## Decision

Every top-level request field, message key, content block type, tool field, thinking type and display
value, stop reason and stream delta type is classed, per client protocol, in one table in code:

- **translated** — sent upstream in its equivalent form;
- **ignored** — accepted, not sent, and reported (response header, log, status tally);
- **rejected** — refused with `400` naming the field, nothing sent.

A field the table does not know is rejected, because there is no way to tell whether dropping it is
safe. `unknown_fields: warn` downgrades that to ignored for a client newer than the gateway. The
parsers look every field up in the table; a row classed translated with no case in the parser is an
internal error that refuses the request rather than panicking.

The tables are the documentation: `airmate fields` prints them, `docs/translation.md` embeds them and a
test pins the embedding, and weekly jobs compare them with the Anthropic SDK's types and OpenAI's
OpenAPI document so a new field surfaces as an issue.

## Consequences

A client that sends a field airmate has not seen fails loudly instead of subtly. Adding support is one
row plus its handling, in the same change. Safety-relevant fields (`safeguards`, server tools) are
rejected rather than ignored, which costs the client a request but never a silent loss of a control.

## Alternatives considered

- *Pass unknown fields through.* Rejected: a field meant for one provider sent to another is at best
  rejected by it and at worst misinterpreted.
- *Drop what cannot be translated and log it.* Rejected: the client cannot tell, and logs are read
  after the damage.
