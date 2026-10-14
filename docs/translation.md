# How airmate translates

airmate sits between a client that speaks one API and an upstream that speaks another. This
document is the reference for what it does with each field, so that nothing a request says is
dropped without the client being told. The tables below are generated from the same tables the code
uses (`airmate fields --inbound <protocol>`) and a test fails when they drift apart.

- [The model](#the-model)
- [Messages and content](#messages-and-content)
- [Reasoning](#reasoning)
- [Thinking signatures](#thinking-signatures)
- [Stop reasons and usage](#stop-reasons-and-usage)
- [Streaming](#streaming)
- [Prompt caching](#prompt-caching)
- [Errors](#errors)
- [Token counting](#token-counting)
- [Provider flavors](#provider-flavors)
- [The field policy](#the-field-policy)
- [Generated policy tables](#generated-policy-tables)

## The model

Every request is parsed into a protocol-neutral intermediate representation (the IR: turns made of
text, images, documents, tool calls, tool results and reasoning, plus tools, sampling, limits and
reasoning controls). Each upstream protocol has an adapter that builds a request from the IR and
reads the reply back into it, and each client protocol has a parser into the IR and a renderer out of
it. Streaming works the same way: an upstream stream parser emits IR events, and a renderer turns
them into the client's events.

| | Client speaks | Upstream speaks |
|---|---|---|
| Anthropic Messages (`/v1/messages`, `/v1/messages/count_tokens`, `/v1/models`) | yes | yes |
| OpenAI Chat Completions (`/v1/chat/completions`, `/v1/models`) | yes | yes |
| OpenAI Responses (`/v1/responses`) | yes | yes |
| OpenAI embeddings (`/v1/embeddings`) | yes | OpenAI Chat and Responses upstreams |
| Google Gemini (`:generateContent`, `:streamGenerateContent`, `:countTokens`, `/v1beta/models`) | yes | yes |
| AWS Bedrock Converse | no | yes |

Where the client and the upstream speak the same protocol (Anthropic to Anthropic, Chat to Chat,
Responses to Responses) the body is **passed through**: forwarded as the client sent it with the
model mapped, the credential swapped, and the provider's documented unsupported fields stripped and
reported as ignored. Only what routing needs is parsed. A Gemini client reaching a Gemini upstream
goes through the IR instead, because the model is in the URL rather than the body.

Endpoints that are not served answer `404 not_found_error` in the client's error format: Message
Batches, the Files API, the Admin API, and every unlisted path.

## Messages and content

- **System text** is the leading system message (Chat, Anthropic `system`, Responses
  `instructions`, Gemini `systemInstruction`). A mid-conversation system message is kept where the
  upstream allows it, and otherwise moved to the front and reported as ignored
  (`system_position`).
- **Turns** alternate user and assistant. Where an upstream requires alternation (Gemini, Bedrock)
  adjacent turns of one role are merged.
- **Tool calls and results.** A tool call keeps its id. A tool result is matched to its call by id
  (by name, for a Gemini function response that carries no id). A result that answers no call is
  refused. Tool names longer than an upstream accepts are shortened with a stable hash and mapped
  back in the reply.
- **Images** are sent as base64 or, where an upstream takes URLs, as URLs that are passed to the
  upstream and never fetched by airmate. Gemini and Bedrock need bytes, so an image URL is refused for
  them.
- **Documents** become text where they are plain text. PDFs are passed to upstreams that read them;
  elsewhere they are refused rather than silently dropped.
- **Structured output** (`json_object`, JSON Schema) is translated to each upstream's equivalent.
  Bedrock Converse has none airmate can use, so a request asking for it is refused.

## Reasoning

A client asks for reasoning in its own terms: Anthropic `thinking` with a budget or adaptive mode and
`output_config.effort`, OpenAI `reasoning_effort` or Responses `reasoning`, Gemini
`thinkingConfig`. airmate reduces these to a mode (off, on with an optional budget, adaptive) and an
effort level (`low`, `medium`, `high`, `xhigh`, `max`), then writes them for the target:

- **OpenAI** takes `reasoning_effort`; the level is clamped *down* to what the target accepts.
- **Fireworks** takes `thinking` or `reasoning_effort`, never both; with `flavor: fireworks`
  airmate sends `thinking` and reports the effort as ignored.
- **Gemini** takes a `thinkingBudget` on 2.5 models (clamped to the model's range, `-1` for
  adaptive, `0` to switch off where the model allows it) and a `thinkingLevel` on 3 models.
- **Bedrock** takes a thinking budget for Claude models, kept under the output limit.

Reasoning text comes back to the client only when it asked for it. Reasoning is a *summary* where the
upstream provides one; some upstreams return none.

## Thinking signatures

Anthropic clients send reasoning back with a `signature`. airmate signs the reasoning it returns:
`am1.<payload>.<mac>`, where the payload names the upstream that produced the reasoning and may carry
the upstream's own opaque state (an OpenAI encrypted item, a Gemini thought signature, a Bedrock
reasoning signature), and the MAC is HMAC-SHA256 keyed from the gateway token. On the next turn:

- a signature that verifies and whose origin is the target being called is replayed as that
  upstream's reasoning;
- anything else (a foreign upstream, a changed token, a signature airmate did not make) is dropped
  and reported as ignored (`thinking_unverified`, `thinking_foreign_origin`), because passing it on
  would make the upstream reject the request.

The payload is signed, not encrypted: a client can read reasoning it was told was omitted. Rotating
the token stops old signatures verifying; the next turn proceeds without the reasoning.

## Stop reasons and usage

| IR finish | Anthropic | Chat | Responses | Gemini |
|---|---|---|---|---|
| stop | `end_turn` | `stop` | `completed` | `STOP` |
| length | `max_tokens` | `length` | `incomplete` | `MAX_TOKENS` |
| tool calls | `tool_use` | `tool_calls` | `completed` with calls | `STOP` |
| refusal | `refusal` | `content_filter` | `incomplete` (content filter) | `SAFETY` |

`stop_sequence` is never reported: upstreams do not say which sequence matched, so a stop on one
reads as `end_turn`.

Usage is kept in the Anthropic shape internally: `input_tokens` excludes cached tokens, which are
reported separately, and reasoning tokens are a subset of output. It is rendered in each client's own
convention (OpenAI's `prompt_tokens` includes cached tokens; Gemini's `candidatesTokenCount` excludes
thinking tokens). No OpenAI-protocol upstream reports cache writes.

## Streaming

Upstream bytes become IR events and then client events, with these guarantees:

- Output is always a **valid sequence** for the client's protocol: for Anthropic, exactly one
  `message_start`, content blocks properly nested with ascending indices, each started block
  stopped, then `message_delta` and `message_stop` (or an `error`).
- **Interleaved** upstream events (parallel tool calls streamed together) are buffered and emitted
  sequentially, up to a cap (`server.buffer_cap_bytes`); past it the stream fails rather than
  growing without bound.
- A stream that **stops early never claims to have finished**: the client gets an `error` event.
- Splitting the upstream bytes anywhere gives identical output. The golden streams in
  `airmate-core/tests/fixtures` are tested at every byte offset.
- While the upstream is silent a keep-alive (`ping`, or an SSE comment) goes out every
  `server.keepalive_secs`.
- Once the first byte has been sent to the client the request **never fails over** to another
  target; it ends in an error event instead (see [failover](operator-guide.md#failover)).

## Prompt caching

`cache_control` anywhere in an Anthropic request sets a `prompt_cache_key` on upstreams that take one
(derived from the model, system text and tools), and the router prefers the same target for the same
key. Gemini caches implicitly and Bedrock needs cache points airmate does not place, so there the
hint is reported as ignored. Cache reads are reported in usage where the upstream reports them; cache
writes are not reported by any OpenAI-protocol upstream.

## Errors

Upstream failures are mapped to the client's own error format and keep their meaning:

| Upstream | Client sees |
|---|---|
| 400 invalid request | `invalid_request_error` (400), with the upstream's message |
| context overflow | `prompt is too long` (so a client compacts), or a message explaining that `max_tokens` is what does not fit |
| 401 / 403 from the upstream | `permission_error` naming the upstream and the setting that holds its key |
| 404 | `not_found_error` |
| 429 | `rate_limit_error` with `Retry-After` |
| 5xx | `api_error`, or `overloaded_error` for 503 / 529 |
| timeout | `timeout_error` (504) |
| unreachable | `api_error` (502) naming the upstream |

The envelope is the client's: Anthropic `{"type":"error","error":{…}}`, OpenAI
`{"error":{"message","type","code","param"}}`, Gemini `{"error":{"code","message","status"}}`.
Upstream error text is scrubbed of every upstream key and the gateway token before it reaches the
client, the logs or the status page.

## Token counting

`POST /v1/messages/count_tokens` (and Gemini's `:countTokens`) is an **estimate** unless the upstream
can count: an Anthropic upstream is asked, anything else is estimated by a character heuristic and
marked with `x-airmate-token-estimate: heuristic`. Near a context limit the estimate can be off by a
margin that matters.

## Provider flavors

A flavor captures how one provider's API departs from the standard for its protocol. Facts are as
documented on the date shown and carry the status in the last column.

| Flavor | Behaviour | Documented | Verified |
|---|---|---|---|
| `generic` | OpenAI Chat as most servers implement it: `max_tokens` (`max_completion_tokens` for GPT-5 and o-series names), streamed usage asked for with `stream_options.include_usage`, `top_k` sent, effort `low`–`high`, no stop-sequence cap, reasoning replayed as `reasoning_content`. | 2026-10-14 | by tests against scripted servers |
| `openai` | `max_completion_tokens`; GPT-5 and o-series accept only the default `temperature` and `top_p` (a client's value is dropped and reported); at most 4 stop sequences; strict structured output; effort up to `xhigh`; `top_k` is not sent; reasoning is not replayed. | 2026-10-14 | not yet against the live API |
| `fireworks` | `max_tokens`; usage is always sent on a stream; `thinking` **or** `reasoning_effort`, never both; at most 4 stop sequences; strict structured output; `top_k` sent; every effort level; `context_length_exceeded_behavior: error` so an overflow errors as Anthropic would; on its Anthropic-compatible surface `inference_geo` and the tool fields `cache_control`, `eager_input_streaming`, `allowed_callers` and `input_examples` are stripped and reported. | 2026-10-14 | not yet against the live API |

Gemini and Bedrock upstreams take no flavor.

A quirk is only called *verified* once its live test (`tests/live.rs`, see the
[operator guide](operator-guide.md#checking-an-upstream-with-doctor)) has passed against the real
service. The weekly `provider-doc-drift` job watches the pages these rest on.

## The field policy

The load-bearing safety rule: **never silently drop a field that changes behaviour.** Every
top-level request field, message key, content block type, tool field, thinking type and display,
stop reason and stream delta type is classed, per client protocol, in one table in code:

- **translated** — sent upstream in its equivalent form;
- **ignored** — accepted but not sent, and reported in the `x-airmate-ignored` response header and
  the request log;
- **rejected** — `400 invalid_request_error` naming the field, and nothing is sent upstream.

A field the table does not know is rejected, because there is no way to tell whether dropping it is
safe. A client newer than the gateway can set `unknown_fields: warn` to have unknown fields ignored
and reported instead. The weekly drift checks compare the tables with the Anthropic SDK's types and
OpenAI's OpenAPI document, so a new field shows up as an issue rather than as a failed request.

Rejecting `safeguards` costs Claude Code one failed request per session in auto permission mode,
after which auto mode runs without Anthropic's server-side classifier. That is inherent in using a
model Anthropic does not serve.

## Generated policy tables

Regenerate with `UPDATE_DOCS=1 cargo test --test docs`.

### Anthropic Messages (client side)

<!-- BEGIN policy:anthropic -->

#### Request fields

| Name | Class | Handling |
|---|---|---|
| `model` | translated | mapped to an upstream model |
| `max_tokens` | translated | `max_tokens`, `max_completion_tokens` (OpenAI) or `max_output_tokens` (Responses); 0 is rejected |
| `messages` | translated | chat messages / Responses input items |
| `system` | translated | a leading system message / Responses `instructions` |
| `stop_sequences` | translated | `stop`; more than 4 rejected on OpenAI and Fireworks; rejected on the Responses target |
| `stream` | translated | upstream streaming, re-framed as Anthropic events |
| `temperature` | translated | dropped (with a warning) for GPT-5/o-series models |
| `top_p` | translated | dropped (with a warning) for GPT-5/o-series models |
| `top_k` | translated | sent to Fireworks and generic upstreams; ignored on OpenAI and Responses |
| `tools` | translated | custom tools become functions; see the tool tables |
| `tool_choice` | translated | `auto`, `any`->`required`, `tool`->named function, `none`; `disable_parallel_tool_use`->`parallel_tool_calls: false` |
| `thinking` | translated | upstream reasoning controls (5.2) |
| `output_config` | translated | `effort`->reasoning effort, `format`->JSON-schema response format; other keys rejected |
| `cache_control` | translated | sets `prompt_cache_key` where the upstream takes one, else ignored |
| `metadata` | ignored | no upstream equivalent |
| `service_tier` | ignored | no upstream equivalent |
| `inference_geo` | ignored | no upstream equivalent |
| `diagnostics` | ignored | no upstream equivalent |
| `speed` | ignored | no upstream equivalent |
| `user_profile_id` | ignored | Anthropic account routing |
| `workspace_id` | ignored | Anthropic account routing |
| `context_management` | ignored | the client's history is sent whole; an overflow returns `prompt is too long` |
| `compaction` | ignored | the client's history is sent whole; an overflow returns `prompt is too long` |
| `fallbacks` | ignored | only the routed target is called (airmate's own fallback applies) |
| `fallback_credit_token` | ignored | only the routed target is called |
| `container` | rejected | server-side code execution has no upstream equivalent |
| `safeguards` | rejected | server-side tool-use classifiers cannot run upstream, and a safety control is never dropped silently; Claude Code retries without it |
| `mcp_servers` | rejected | the MCP connector has no upstream equivalent |

#### Message keys

| Name | Class | Handling |
|---|---|---|
| `role` | translated | `user`, `assistant`, or a mid-conversation `system` message |
| `content` | translated | a string or a list of content blocks |
| `output_config` | translated | on a mid-conversation system message, sets that turn's effort |

#### Thinking fields

| Name | Class | Handling |
|---|---|---|
| `type` | translated | see the thinking types |
| `budget_tokens` | translated | bucketed into a reasoning effort, or sent as the Fireworks budget |
| `display` | translated | see the display values |

#### Thinking types

| Name | Class | Handling |
|---|---|---|
| `enabled` | translated | reasoning on, with `budget_tokens` |
| `adaptive` | translated | reasoning on, effort from `output_config.effort` or the model default |
| `between_tools` | translated | treated as `adaptive` |
| `disabled` | translated | Fireworks `thinking: {"type":"disabled"}`; nothing elsewhere |

#### Thinking display values

| Name | Class | Handling |
|---|---|---|
| `summarized` | translated | reasoning returned as `thinking` blocks |
| `omitted` | translated | empty `thinking` blocks; the hidden text rides in the signature |
| `updates` | translated | reasoning returned as `thinking` blocks |

#### Output config keys

| Name | Class | Handling |
|---|---|---|
| `effort` | translated | reasoning effort, clamped to what the target accepts |
| `format` | translated | JSON-schema response format |

#### Content blocks

| Name | Class | Handling |
|---|---|---|
| `text` | translated | text |
| `image` | translated | `image_url` (base64 data URI or URL); `file` sources rejected |
| `document` | translated | plain-text and inline-content documents become text; PDF, URL and file sources rejected |
| `search_result` | translated | rendered as text |
| `thinking` | translated | replayed as upstream reasoning when the signature verifies and the origin matches (5.3), else dropped |
| `redacted_thinking` | ignored | encrypted for Anthropic; no upstream can read it |
| `tool_use` | translated | assistant `tool_calls` / Responses `function_call` |
| `tool_result` | translated | `role: tool` message / Responses `function_call_output` |
| `tool_reference` | translated | rendered as text; every tool is sent eagerly |
| `server_tool_use` | rejected | server tools have no upstream equivalent |
| `web_search_tool_result` | rejected | server tools have no upstream equivalent |
| `web_fetch_tool_result` | rejected | server tools have no upstream equivalent |
| `code_execution_tool_result` | rejected | server tools have no upstream equivalent |
| `bash_code_execution_tool_result` | rejected | server tools have no upstream equivalent |
| `text_editor_code_execution_tool_result` | rejected | server tools have no upstream equivalent |
| `tool_search_tool_result` | rejected | server tools have no upstream equivalent |
| `container_upload` | rejected | server-side code execution has no upstream equivalent |
| `advisor_tool_result` | rejected | server tools have no upstream equivalent |
| `mcp_tool_use` | rejected | the MCP connector has no upstream equivalent |
| `mcp_tool_result` | rejected | the MCP connector has no upstream equivalent |
| `mcp_tool_listing` | rejected | the MCP connector has no upstream equivalent |
| `compaction` | translated | the summary becomes text, so the context it carries is not lost |
| `fallback` | ignored | marks a fallback hop inside Anthropic's service; not part of the prompt |
| `tool_addition` | ignored | every tool is sent eagerly |
| `tool_removal` | ignored | every tool is sent eagerly |

#### Content block fields

| Name | Class | Handling |
|---|---|---|
| `compaction.content` | translated | the summary of the compacted context; `null` (a failed compaction) is a no-op |
| `compaction.cache_control` | translated | contributes to `prompt_cache_key` |
| `compaction.encrypted_content` | ignored | opaque to every upstream |
| `compaction.signature` | ignored | opaque to every upstream |
| `compaction.tool_changes` | ignored | every tool is sent eagerly |
| `text.text` | translated | the text |
| `text.cache_control` | translated | contributes to `prompt_cache_key` |
| `text.citations` | ignored | upstreams return no citations |
| `image.source` | translated | the image |
| `image.cache_control` | translated | contributes to `prompt_cache_key` |
| `image.source.type=base64` | translated | a base64 data URI |
| `image.source.type=url` | translated | passed to the upstream, never fetched by the gateway |
| `image.source.type=file` | rejected | the Files API is not served |
| `document.source` | translated | the document |
| `document.cache_control` | translated | contributes to `prompt_cache_key` |
| `document.title` | ignored | no upstream equivalent |
| `document.context` | ignored | no upstream equivalent |
| `document.citations` | ignored | upstreams return no citations |
| `document.source.type=text` | translated | becomes text |
| `document.source.type=content` | translated | becomes text |
| `document.source.type=base64` | rejected | PDF documents are not supported |
| `document.source.type=url` | rejected | URL documents are not supported |
| `document.source.type=file` | rejected | the Files API is not served |
| `search_result.source` | translated | rendered as text |
| `search_result.title` | translated | rendered as text |
| `search_result.content` | translated | rendered as text |
| `search_result.cache_control` | translated | contributes to `prompt_cache_key` |
| `search_result.citations` | ignored | upstreams return no citations |
| `thinking.thinking` | translated | the reasoning text |
| `thinking.signature` | translated | verified against the gateway key (5.3) |
| `redacted_thinking.data` | ignored | encrypted for Anthropic; no upstream can read it |
| `tool_use.id` | translated | the call id |
| `tool_use.name` | translated | names over 64 characters are shortened and mapped back |
| `tool_use.input` | translated | the call arguments |
| `tool_use.cache_control` | translated | contributes to `prompt_cache_key` |
| `tool_use.caller` | ignored | only direct calls exist upstream |
| `tool_result.tool_use_id` | translated | the id of the call this answers |
| `tool_result.content` | translated | text and images |
| `tool_result.is_error` | translated | an error marker in the result text |
| `tool_result.cache_control` | translated | contributes to `prompt_cache_key` |
| `tool_reference.tool_name` | translated | rendered as text |

#### Tool fields

| Name | Class | Handling |
|---|---|---|
| `type` | translated | absent or `custom`; any Anthropic-defined tool type is rejected |
| `name` | translated | names over 64 characters are shortened and mapped back |
| `description` | translated | function description |
| `input_schema` | translated | function parameters |
| `strict` | translated | function `strict` |
| `input_examples` | translated | appended to the description |
| `cache_control` | translated | contributes to `prompt_cache_key` |
| `defer_loading` | ignored | every tool is sent eagerly |
| `eager_input_streaming` | ignored | arguments always stream |
| `allowed_callers` | ignored | only direct calls exist upstream |

#### Tool choice fields

| Name | Class | Handling |
|---|---|---|
| `type` | translated | `auto`, `any`, `tool` or `none` |
| `name` | translated | the named tool |
| `disable_parallel_tool_use` | translated | `parallel_tool_calls: false` |

#### Stop reasons

| Name | Class | Handling |
|---|---|---|
| `end_turn` | translated | `finish_reason: stop` |
| `max_tokens` | translated | `finish_reason: length` |
| `stop_sequence` | ignored | upstreams do not say which sequence matched; reported as `end_turn` |
| `tool_use` | translated | `finish_reason: tool_calls` |
| `pause_turn` | ignored | only server tools pause |
| `compaction` | ignored | only Anthropic's own service compacts |
| `refusal` | translated | `content_filter` or an upstream `refusal` |
| `model_context_window_exceeded` | ignored | upstreams return an error, answered as `prompt is too long` |

#### Stream deltas

| Name | Class | Handling |
|---|---|---|
| `text_delta` | translated | text |
| `input_json_delta` | translated | tool arguments |
| `thinking_delta` | translated | reasoning text |
| `signature_delta` | translated | the gateway's signature |
| `citations_delta` | ignored | upstreams return no citations |
| `compaction_delta` | ignored | only Anthropic's own service compacts |
<!-- END policy:anthropic -->

### OpenAI Chat Completions (client side)

<!-- BEGIN policy:chat -->

#### Request fields

| Name | Class | Handling |
|---|---|---|
| `model` | translated | mapped to an upstream model |
| `messages` | translated | the conversation |
| `max_tokens` | translated | the output token limit (`max_completion_tokens` wins when both are set) |
| `max_completion_tokens` | translated | the output token limit |
| `temperature` | translated | dropped (with a warning) for GPT-5/o-series models |
| `top_p` | translated | dropped (with a warning) for GPT-5/o-series models |
| `top_k` | translated | an extension some servers take; sent to Fireworks and generic upstreams |
| `stream` | translated | upstream streaming, re-framed as Chat Completions chunks |
| `stream_options` | translated | `include_usage` adds a final usage chunk |
| `stop` | translated | a string or up to 4 strings |
| `tools` | translated | function tools |
| `tool_choice` | translated | `none`, `auto`, `required` or a named function |
| `parallel_tool_calls` | translated | `false` disables parallel calls |
| `response_format` | translated | `text`, `json_object` or `json_schema` |
| `reasoning_effort` | translated | the reasoning effort, clamped to what the target accepts |
| `thinking` | translated | an extension (Anthropic- and Fireworks-style) for reasoning on or off with a budget |
| `seed` | translated | sent where the upstream takes one |
| `frequency_penalty` | translated | sent where the upstream takes one |
| `presence_penalty` | translated | sent where the upstream takes one |
| `prompt_cache_key` | translated | sent where the upstream takes one |
| `n` | translated | only `1`; more than one choice is rejected |
| `user` | ignored | no upstream equivalent |
| `metadata` | ignored | no upstream equivalent |
| `store` | ignored | airmate stores nothing |
| `service_tier` | ignored | no upstream equivalent |
| `safety_identifier` | ignored | no upstream equivalent |
| `verbosity` | ignored | no upstream equivalent |
| `prediction` | ignored | a latency hint; the answer is the same |
| `logit_bias` | rejected | changes sampling and has no upstream equivalent |
| `logprobs` | rejected | log probabilities cannot be produced |
| `top_logprobs` | rejected | log probabilities cannot be produced |
| `modalities` | rejected | audio output is not supported |
| `audio` | rejected | audio output is not supported |
| `web_search_options` | rejected | server-side features have no upstream equivalent |
| `functions` | rejected | deprecated by OpenAI; use `tools` |
| `function_call` | rejected | deprecated by OpenAI; use `tools` |

#### Message keys

| Name | Class | Handling |
|---|---|---|
| `role` | translated | `system`, `developer`, `user`, `assistant` or `tool` |
| `content` | translated | a string or a list of content parts |
| `tool_calls` | translated | assistant function calls |
| `tool_call_id` | translated | the call a tool message answers |
| `refusal` | translated | an assistant refusal, kept as text |
| `name` | ignored | no upstream equivalent |
| `annotations` | ignored | citations are not produced upstream |
| `reasoning_content` | ignored | reasoning is only replayed when its signature verifies; Chat Completions carries none |
| `reasoning` | ignored | reasoning is only replayed when its signature verifies; Chat Completions carries none |
| `function_call` | rejected | deprecated by OpenAI; use `tools` |
| `audio` | rejected | audio is not supported |

#### Thinking fields

| Name | Class | Handling |
|---|---|---|
| `type` | translated | `enabled`, `adaptive` or `disabled` |
| `budget_tokens` | translated | the thinking budget |

#### Content blocks

| Name | Class | Handling |
|---|---|---|
| `text` | translated | text |
| `image_url` | translated | an image, as a data URI or a URL passed to the upstream |
| `refusal` | translated | an assistant refusal, kept as text |
| `input_audio` | rejected | audio input is not supported |
| `file` | rejected | file inputs are not supported |

#### Content block fields

| Name | Class | Handling |
|---|---|---|
| `text.text` | translated | the text |
| `image_url.image_url` | translated | the image |
| `image_url.image_url.url` | translated | the image URL or data URI |
| `image_url.image_url.detail` | ignored | no upstream equivalent |
| `refusal.refusal` | translated | the refusal text |

#### Tool fields

| Name | Class | Handling |
|---|---|---|
| `type` | translated | `function`; other tool types are rejected |
| `function` | translated | the function definition |
| `function.name` | translated | names over 64 characters are shortened and mapped back |
| `function.description` | translated | the description |
| `function.parameters` | translated | the JSON Schema of the arguments |
| `function.strict` | translated | strict schema adherence where the upstream supports it |

#### Tool choice fields

| Name | Class | Handling |
|---|---|---|
| `type` | translated | `function` |
| `function` | translated | the named function |
| `function.name` | translated | the function to call |

#### Stop reasons

| Name | Class | Handling |
|---|---|---|
| `stop` | translated | `end_turn` |
| `length` | translated | `max_tokens` |
| `tool_calls` | translated | `tool_use` |
| `content_filter` | translated | `refusal` |
<!-- END policy:chat -->

### OpenAI Responses (client side)

<!-- BEGIN policy:responses -->

#### Request fields

| Name | Class | Handling |
|---|---|---|
| `model` | translated | mapped to an upstream model |
| `input` | translated | a string or a list of items |
| `instructions` | translated | a leading system message |
| `max_output_tokens` | translated | the output token limit |
| `temperature` | translated | dropped (with a warning) for GPT-5/o-series models |
| `top_p` | translated | dropped (with a warning) for GPT-5/o-series models |
| `stream` | translated | upstream streaming, re-framed as Responses events |
| `tools` | translated | function tools |
| `tool_choice` | translated | `none`, `auto`, `required` or a named function |
| `parallel_tool_calls` | translated | `false` disables parallel calls |
| `reasoning` | translated | effort and summary |
| `text` | translated | `format` selects text, json_object or json_schema output |
| `include` | translated | `reasoning.encrypted_content` is always honoured; other values are ignored |
| `prompt_cache_key` | translated | sent where the upstream takes one |
| `store` | ignored | airmate stores nothing |
| `metadata` | ignored | no upstream equivalent |
| `user` | ignored | no upstream equivalent |
| `safety_identifier` | ignored | no upstream equivalent |
| `service_tier` | ignored | no upstream equivalent |
| `truncation` | ignored | the history is sent whole; an overflow returns `prompt is too long` |
| `max_tool_calls` | ignored | no upstream equivalent |
| `stream_options` | ignored | no upstream equivalent |
| `previous_response_id` | rejected | airmate keeps no server-side conversation state; send the full history |
| `conversation` | rejected | airmate keeps no server-side conversation state; send the full history |
| `background` | rejected | background responses need server-side state |
| `prompt` | rejected | stored prompt templates need server-side state |
| `top_logprobs` | rejected | log probabilities cannot be produced |

#### Input item types

| Name | Class | Handling |
|---|---|---|
| `message` | translated | a conversation message |
| `function_call` | translated | an assistant function call |
| `function_call_output` | translated | a tool result |
| `reasoning` | translated | replayed as upstream reasoning when its signature verifies and the origin matches, else dropped |
| `item_reference` | rejected | airmate keeps no server-side conversation state; send the full history |
| `web_search_call` | rejected | server-side tools and state have no upstream equivalent |
| `file_search_call` | rejected | server-side tools and state have no upstream equivalent |
| `computer_call` | rejected | server-side tools and state have no upstream equivalent |
| `computer_call_output` | rejected | server-side tools and state have no upstream equivalent |
| `code_interpreter_call` | rejected | server-side tools and state have no upstream equivalent |
| `image_generation_call` | rejected | server-side tools and state have no upstream equivalent |
| `local_shell_call` | rejected | server-side tools and state have no upstream equivalent |
| `local_shell_call_output` | rejected | server-side tools and state have no upstream equivalent |
| `mcp_call` | rejected | server-side tools and state have no upstream equivalent |
| `mcp_list_tools` | rejected | server-side tools and state have no upstream equivalent |
| `mcp_approval_request` | rejected | server-side tools and state have no upstream equivalent |
| `mcp_approval_response` | rejected | server-side tools and state have no upstream equivalent |
| `custom_tool_call` | rejected | server-side tools and state have no upstream equivalent |
| `custom_tool_call_output` | rejected | server-side tools and state have no upstream equivalent |

#### Content part types

| Name | Class | Handling |
|---|---|---|
| `input_text` | translated | text |
| `output_text` | translated | assistant text |
| `refusal` | translated | an assistant refusal, kept as text |
| `input_image` | translated | an image, as a data URI or a URL passed to the upstream; file ids are rejected |
| `input_file` | rejected | file inputs are not supported |
| `input_audio` | rejected | audio input is not supported |

#### Thinking fields

| Name | Class | Handling |
|---|---|---|
| `effort` | translated | the reasoning effort, clamped to what the target accepts |
| `summary` | translated | asks for reasoning to be returned |
| `generate_summary` | translated | asks for reasoning to be returned (deprecated name) |

#### Content block fields

| Name | Class | Handling |
|---|---|---|
| `message.type` | translated | `message` |
| `message.role` | translated | `user`, `assistant`, `system` or `developer` |
| `message.content` | translated | a string or a list of content parts |
| `message.id` | translated | accepted; the upstream assigns its own ids |
| `message.status` | translated | accepted; the upstream assigns its own ids |
| `function_call.type` | translated | `function_call` |
| `function_call.call_id` | translated | the call id |
| `function_call.name` | translated | names over 64 characters are shortened and mapped back |
| `function_call.arguments` | translated | the call arguments, as a JSON string |
| `function_call.id` | translated | accepted; the upstream assigns its own ids |
| `function_call.status` | translated | accepted; the upstream assigns its own ids |
| `function_call_output.type` | translated | `function_call_output` |
| `function_call_output.call_id` | translated | the id of the call this answers |
| `function_call_output.output` | translated | a string, or a list of text and image parts |
| `function_call_output.id` | translated | accepted; the upstream assigns its own ids |
| `function_call_output.status` | translated | accepted; the upstream assigns its own ids |
| `reasoning.type` | translated | `reasoning` |
| `reasoning.summary` | translated | the reasoning text |
| `reasoning.encrypted_content` | translated | the gateway's signature, which carries the upstream's encrypted content |
| `reasoning.id` | translated | accepted; the upstream assigns its own ids |
| `reasoning.status` | translated | accepted; the upstream assigns its own ids |
| `reasoning.content` | ignored | raw reasoning text is not replayed |
| `input_text.text` | translated | the text |
| `output_text.text` | translated | the text |
| `output_text.annotations` | ignored | citations are not produced upstream |
| `output_text.logprobs` | ignored | log probabilities are not produced |
| `refusal.refusal` | translated | the refusal text |
| `input_image.image_url` | translated | the image |
| `input_image.detail` | ignored | no upstream equivalent |
| `input_image.file_id` | rejected | file ids need server-side storage |

#### Tool fields

| Name | Class | Handling |
|---|---|---|
| `type` | translated | `function`; other tool types are rejected |
| `name` | translated | names over 64 characters are shortened and mapped back |
| `description` | translated | the description |
| `parameters` | translated | the JSON Schema of the arguments |
| `strict` | translated | strict schema adherence where the upstream supports it |

#### Tool choice fields

| Name | Class | Handling |
|---|---|---|
| `type` | translated | `function` |
| `name` | translated | the function to call |
<!-- END policy:responses -->

### Google Gemini (client side)

<!-- BEGIN policy:gemini -->

#### Request fields

| Name | Class | Handling |
|---|---|---|
| `contents` | translated | the conversation |
| `systemInstruction` | translated | the system prompt |
| `tools` | translated | function declarations |
| `toolConfig` | translated | `functionCallingConfig` selects the tool-choice mode |
| `generationConfig` | translated | sampling, limits, structured output and thinking (see the `generationConfig.` rows) |
| `safetySettings` | ignored | no upstream equivalent |
| `labels` | ignored | no upstream equivalent |
| `serviceTier` | ignored | no upstream equivalent |
| `cachedContent` | rejected | cached content lives in Google's service and cannot be read by another upstream |
| `generationConfig.temperature` | translated | sent where the upstream takes one |
| `generationConfig.topP` | translated | sent where the upstream takes one |
| `generationConfig.topK` | translated | sent to Gemini, Fireworks and generic upstreams |
| `generationConfig.candidateCount` | translated | only `1`; more than one candidate is rejected |
| `generationConfig.maxOutputTokens` | translated | the output token limit |
| `generationConfig.stopSequences` | translated | stop sequences |
| `generationConfig.seed` | translated | sent where the upstream takes one |
| `generationConfig.presencePenalty` | translated | sent where the upstream takes one |
| `generationConfig.frequencyPenalty` | translated | sent where the upstream takes one |
| `generationConfig.responseMimeType` | translated | `application/json` asks for JSON; `text/plain` is the default |
| `generationConfig.responseSchema` | translated | an OpenAPI-subset schema for structured output |
| `generationConfig.responseJsonSchema` | translated | a JSON Schema for structured output |
| `generationConfig.thinkingConfig` | translated | reasoning on, off or with a budget (see the thinking rows) |
| `generationConfig.responseModalities` | translated | only `TEXT`; image or audio output is rejected |
| `generationConfig.responseLogprobs` | rejected | log probabilities cannot be produced |
| `generationConfig.logprobs` | rejected | log probabilities cannot be produced |
| `generationConfig.speechConfig` | rejected | audio output is not supported |
| `generationConfig.mediaResolution` | ignored | no upstream equivalent |
| `generationConfig.audioTimestamp` | ignored | no upstream equivalent |
| `generationConfig.enableEnhancedCivicAnswers` | ignored | no upstream equivalent |

#### Message keys

| Name | Class | Handling |
|---|---|---|
| `role` | translated | `user` or `model` (a missing role is `user`) |
| `parts` | translated | the content parts |

#### Thinking fields

| Name | Class | Handling |
|---|---|---|
| `includeThoughts` | translated | thought summaries are returned only when set |
| `thinkingBudget` | translated | `-1` lets the model decide, `0` turns thinking off, otherwise a token budget |
| `thinkingLevel` | translated | an effort level (`minimal`, `low`, `medium`, `high`) |

#### Content blocks

| Name | Class | Handling |
|---|---|---|
| `text` | translated | text |
| `inlineData` | translated | an image, a PDF or text, as base64 |
| `fileData` | rejected | file URIs point at Google's service; send the bytes inline |
| `functionCall` | translated | a tool call made by the model |
| `functionResponse` | translated | a tool result |
| `executableCode` | rejected | code execution is a server-side tool |
| `codeExecutionResult` | rejected | code execution is a server-side tool |

#### Content block fields

| Name | Class | Handling |
|---|---|---|
| `text.thought` | ignored | reasoning is not replayed on Gemini requests |
| `thoughtSignature` | ignored | reasoning is not replayed on Gemini requests |
| `videoMetadata` | ignored | no upstream equivalent |
| `functionDeclaration.name` | translated | the function name |
| `functionDeclaration.description` | translated | the function description |
| `functionDeclaration.parameters` | translated | an OpenAPI-subset schema, converted to JSON Schema |
| `functionDeclaration.parametersJsonSchema` | translated | a JSON Schema for the arguments |
| `functionDeclaration.response` | ignored | no upstream equivalent |
| `functionDeclaration.responseJsonSchema` | ignored | no upstream equivalent |
| `functionDeclaration.behavior` | ignored | asynchronous function calls are not supported |

#### Tool fields

| Name | Class | Handling |
|---|---|---|
| `functionDeclarations` | translated | function tools |
| `googleSearch` | rejected | server-side tools are not executed by airmate |
| `googleSearchRetrieval` | rejected | server-side tools are not executed by airmate |
| `codeExecution` | rejected | server-side tools are not executed by airmate |
| `urlContext` | rejected | server-side tools are not executed by airmate |
| `retrieval` | rejected | server-side tools are not executed by airmate |
| `computerUse` | rejected | server-side tools are not executed by airmate |
| `fileSearch` | rejected | server-side tools are not executed by airmate |
| `googleMaps` | rejected | server-side tools are not executed by airmate |

#### Tool choice fields

| Name | Class | Handling |
|---|---|---|
| `mode` | translated | `AUTO`, `ANY`, `NONE` or `VALIDATED` |
| `allowedFunctionNames` | translated | one name forces that function; several are treated as `ANY` and reported |

#### Stop reasons

| Name | Class | Handling |
|---|---|---|
| `STOP` | translated | from `end_turn` or `tool_use` |
| `MAX_TOKENS` | translated | from `max_tokens` |
| `SAFETY` | translated | from a refusal or content filter |
<!-- END policy:gemini -->
