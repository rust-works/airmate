//! The Anthropic inbound field policy, as of 2026-10-14.
//!
//! `airmate fields` prints this table. Adding a translation is a row here plus its handling in the
//! parser, in the same change.

use super::{
    entry,
    Class::{Ignored, Rejected, Translated},
    Entry,
    Group::{
        BlockField, ContentBlock, MessageKey, OutputConfigKey, Request, StopReason, StreamDelta,
        ThinkingDisplay, ThinkingField, ThinkingType, ToolChoiceField, ToolField,
    },
};

const SERVER_TOOLS: &str = "server tools have no upstream equivalent";
const SERVER_CODE: &str = "server-side code execution has no upstream equivalent";
const NO_EQUIVALENT: &str = "no upstream equivalent";
const MCP: &str = "the MCP connector has no upstream equivalent";
const HISTORY_WHOLE: &str =
    "the client's history is sent whole; an overflow returns `prompt is too long`";

/// The Anthropic inbound policy table.
pub const TABLE: &[Entry] = &[
    // Request fields
    entry(Request, "model", Translated, "mapped to an upstream model"),
    entry(Request, "max_tokens", Translated, "`max_tokens`, `max_completion_tokens` (OpenAI) or `max_output_tokens` (Responses); 0 is rejected"),
    entry(Request, "messages", Translated, "chat messages / Responses input items"),
    entry(Request, "system", Translated, "a leading system message / Responses `instructions`"),
    entry(Request, "stop_sequences", Translated, "`stop`; more than 4 rejected on OpenAI and Fireworks; rejected on the Responses target"),
    entry(Request, "stream", Translated, "upstream streaming, re-framed as Anthropic events"),
    entry(Request, "temperature", Translated, "dropped (with a warning) for GPT-5/o-series models"),
    entry(Request, "top_p", Translated, "dropped (with a warning) for GPT-5/o-series models"),
    entry(Request, "top_k", Translated, "sent to Fireworks and generic upstreams; ignored on OpenAI and Responses"),
    entry(Request, "tools", Translated, "custom tools become functions; see the tool tables"),
    entry(Request, "tool_choice", Translated, "`auto`, `any`->`required`, `tool`->named function, `none`; `disable_parallel_tool_use`->`parallel_tool_calls: false`"),
    entry(Request, "thinking", Translated, "upstream reasoning controls (5.2)"),
    entry(Request, "output_config", Translated, "`effort`->reasoning effort, `format`->JSON-schema response format; other keys rejected"),
    entry(Request, "cache_control", Translated, "sets `prompt_cache_key` where the upstream takes one, else ignored"),
    entry(Request, "metadata", Ignored, NO_EQUIVALENT),
    entry(Request, "service_tier", Ignored, NO_EQUIVALENT),
    entry(Request, "inference_geo", Ignored, NO_EQUIVALENT),
    entry(Request, "diagnostics", Ignored, NO_EQUIVALENT),
    entry(Request, "speed", Ignored, NO_EQUIVALENT),
    entry(Request, "user_profile_id", Ignored, "Anthropic account routing"),
    entry(Request, "workspace_id", Ignored, "Anthropic account routing"),
    entry(Request, "context_management", Ignored, HISTORY_WHOLE),
    entry(Request, "compaction", Ignored, HISTORY_WHOLE),
    entry(Request, "fallbacks", Ignored, "only the routed target is called (airmate's own fallback applies)"),
    entry(Request, "fallback_credit_token", Ignored, "only the routed target is called"),
    entry(Request, "container", Rejected, SERVER_CODE),
    entry(Request, "safeguards", Rejected, "server-side tool-use classifiers cannot run upstream, and a safety control is never dropped silently; Claude Code retries without it"),
    entry(Request, "mcp_servers", Rejected, "the MCP connector has no upstream equivalent"),
    // Message keys
    entry(MessageKey, "role", Translated, "`user`, `assistant`, or a mid-conversation `system` message"),
    entry(MessageKey, "content", Translated, "a string or a list of content blocks"),
    entry(MessageKey, "output_config", Translated, "on a mid-conversation system message, sets that turn's effort"),
    // Thinking
    entry(ThinkingField, "type", Translated, "see the thinking types"),
    entry(ThinkingField, "budget_tokens", Translated, "bucketed into a reasoning effort, or sent as the Fireworks budget"),
    entry(ThinkingField, "display", Translated, "see the display values"),
    entry(ThinkingType, "enabled", Translated, "reasoning on, with `budget_tokens`"),
    entry(ThinkingType, "adaptive", Translated, "reasoning on, effort from `output_config.effort` or the model default"),
    entry(ThinkingType, "between_tools", Translated, "treated as `adaptive`"),
    entry(ThinkingType, "disabled", Translated, "Fireworks `thinking: {\"type\":\"disabled\"}`; nothing elsewhere"),
    entry(ThinkingDisplay, "summarized", Translated, "reasoning returned as `thinking` blocks"),
    entry(ThinkingDisplay, "omitted", Translated, "empty `thinking` blocks; the hidden text rides in the signature"),
    entry(ThinkingDisplay, "updates", Translated, "reasoning returned as `thinking` blocks"),
    // output_config keys
    entry(OutputConfigKey, "effort", Translated, "reasoning effort, clamped to what the target accepts"),
    entry(OutputConfigKey, "format", Translated, "JSON-schema response format"),
    // Content blocks
    entry(ContentBlock, "text", Translated, "text"),
    entry(ContentBlock, "image", Translated, "`image_url` (base64 data URI or URL); `file` sources rejected"),
    entry(ContentBlock, "document", Translated, "plain-text and inline-content documents become text; PDF, URL and file sources rejected"),
    entry(ContentBlock, "search_result", Translated, "rendered as text"),
    entry(ContentBlock, "thinking", Translated, "replayed as upstream reasoning when the signature verifies and the origin matches (5.3), else dropped"),
    entry(ContentBlock, "redacted_thinking", Ignored, "encrypted for Anthropic; no upstream can read it"),
    entry(ContentBlock, "tool_use", Translated, "assistant `tool_calls` / Responses `function_call`"),
    entry(ContentBlock, "tool_result", Translated, "`role: tool` message / Responses `function_call_output`"),
    entry(ContentBlock, "tool_reference", Translated, "rendered as text; every tool is sent eagerly"),
    entry(ContentBlock, "server_tool_use", Rejected, SERVER_TOOLS),
    entry(ContentBlock, "web_search_tool_result", Rejected, SERVER_TOOLS),
    entry(ContentBlock, "web_fetch_tool_result", Rejected, SERVER_TOOLS),
    entry(ContentBlock, "code_execution_tool_result", Rejected, SERVER_TOOLS),
    entry(ContentBlock, "bash_code_execution_tool_result", Rejected, SERVER_TOOLS),
    entry(ContentBlock, "text_editor_code_execution_tool_result", Rejected, SERVER_TOOLS),
    entry(ContentBlock, "tool_search_tool_result", Rejected, SERVER_TOOLS),
    entry(ContentBlock, "container_upload", Rejected, SERVER_CODE),
    entry(ContentBlock, "advisor_tool_result", Rejected, SERVER_TOOLS),
    entry(ContentBlock, "mcp_tool_use", Rejected, MCP),
    entry(ContentBlock, "mcp_tool_result", Rejected, MCP),
    entry(ContentBlock, "mcp_tool_listing", Rejected, MCP),
    entry(ContentBlock, "compaction", Translated, "the summary becomes text, so the context it carries is not lost"),
    entry(ContentBlock, "fallback", Ignored, "marks a fallback hop inside Anthropic's service; not part of the prompt"),
    entry(ContentBlock, "tool_addition", Ignored, "every tool is sent eagerly"),
    entry(ContentBlock, "tool_removal", Ignored, "every tool is sent eagerly"),
    // Content block fields, `<type>.<key>`
    entry(BlockField, "compaction.content", Translated, "the summary of the compacted context; `null` (a failed compaction) is a no-op"),
    entry(BlockField, "compaction.cache_control", Translated, "contributes to `prompt_cache_key`"),
    entry(BlockField, "compaction.encrypted_content", Ignored, "opaque to every upstream"),
    entry(BlockField, "compaction.signature", Ignored, "opaque to every upstream"),
    entry(BlockField, "compaction.tool_changes", Ignored, "every tool is sent eagerly"),
    entry(BlockField, "text.text", Translated, "the text"),
    entry(BlockField, "text.cache_control", Translated, "contributes to `prompt_cache_key`"),
    entry(BlockField, "text.citations", Ignored, "upstreams return no citations"),
    entry(BlockField, "image.source", Translated, "the image"),
    entry(BlockField, "image.cache_control", Translated, "contributes to `prompt_cache_key`"),
    entry(BlockField, "image.source.type=base64", Translated, "a base64 data URI"),
    entry(BlockField, "image.source.type=url", Translated, "passed to the upstream, never fetched by the gateway"),
    entry(BlockField, "image.source.type=file", Rejected, "the Files API is not served"),
    entry(BlockField, "document.source", Translated, "the document"),
    entry(BlockField, "document.cache_control", Translated, "contributes to `prompt_cache_key`"),
    entry(BlockField, "document.title", Ignored, NO_EQUIVALENT),
    entry(BlockField, "document.context", Ignored, NO_EQUIVALENT),
    entry(BlockField, "document.citations", Ignored, "upstreams return no citations"),
    entry(BlockField, "document.source.type=text", Translated, "becomes text"),
    entry(BlockField, "document.source.type=content", Translated, "becomes text"),
    entry(BlockField, "document.source.type=base64", Rejected, "PDF documents are not supported"),
    entry(BlockField, "document.source.type=url", Rejected, "URL documents are not supported"),
    entry(BlockField, "document.source.type=file", Rejected, "the Files API is not served"),
    entry(BlockField, "search_result.source", Translated, "rendered as text"),
    entry(BlockField, "search_result.title", Translated, "rendered as text"),
    entry(BlockField, "search_result.content", Translated, "rendered as text"),
    entry(BlockField, "search_result.cache_control", Translated, "contributes to `prompt_cache_key`"),
    entry(BlockField, "search_result.citations", Ignored, "upstreams return no citations"),
    entry(BlockField, "thinking.thinking", Translated, "the reasoning text"),
    entry(BlockField, "thinking.signature", Translated, "verified against the gateway key (5.3)"),
    entry(BlockField, "redacted_thinking.data", Ignored, "encrypted for Anthropic; no upstream can read it"),
    entry(BlockField, "tool_use.id", Translated, "the call id"),
    entry(BlockField, "tool_use.name", Translated, "names over 64 characters are shortened and mapped back"),
    entry(BlockField, "tool_use.input", Translated, "the call arguments"),
    entry(BlockField, "tool_use.cache_control", Translated, "contributes to `prompt_cache_key`"),
    entry(BlockField, "tool_use.caller", Ignored, "only direct calls exist upstream"),
    entry(BlockField, "tool_result.tool_use_id", Translated, "the id of the call this answers"),
    entry(BlockField, "tool_result.content", Translated, "text and images"),
    entry(BlockField, "tool_result.is_error", Translated, "an error marker in the result text"),
    entry(BlockField, "tool_result.cache_control", Translated, "contributes to `prompt_cache_key`"),
    entry(BlockField, "tool_reference.tool_name", Translated, "rendered as text"),
    // Tool fields
    entry(ToolField, "type", Translated, "absent or `custom`; any Anthropic-defined tool type is rejected"),
    entry(ToolField, "name", Translated, "names over 64 characters are shortened and mapped back"),
    entry(ToolField, "description", Translated, "function description"),
    entry(ToolField, "input_schema", Translated, "function parameters"),
    entry(ToolField, "strict", Translated, "function `strict`"),
    entry(ToolField, "input_examples", Translated, "appended to the description"),
    entry(ToolField, "cache_control", Translated, "contributes to `prompt_cache_key`"),
    entry(ToolField, "defer_loading", Ignored, "every tool is sent eagerly"),
    entry(ToolField, "eager_input_streaming", Ignored, "arguments always stream"),
    entry(ToolField, "allowed_callers", Ignored, "only direct calls exist upstream"),
    // tool_choice fields
    entry(ToolChoiceField, "type", Translated, "`auto`, `any`, `tool` or `none`"),
    entry(ToolChoiceField, "name", Translated, "the named tool"),
    entry(ToolChoiceField, "disable_parallel_tool_use", Translated, "`parallel_tool_calls: false`"),
    // Stop reasons
    entry(StopReason, "end_turn", Translated, "`finish_reason: stop`"),
    entry(StopReason, "max_tokens", Translated, "`finish_reason: length`"),
    entry(StopReason, "stop_sequence", Ignored, "upstreams do not say which sequence matched; reported as `end_turn`"),
    entry(StopReason, "tool_use", Translated, "`finish_reason: tool_calls`"),
    entry(StopReason, "pause_turn", Ignored, "only server tools pause"),
    entry(StopReason, "compaction", Ignored, "only Anthropic's own service compacts"),
    entry(StopReason, "refusal", Translated, "`content_filter` or an upstream `refusal`"),
    entry(StopReason, "model_context_window_exceeded", Ignored, "upstreams return an error, answered as `prompt is too long`"),
    // Stream deltas
    entry(StreamDelta, "text_delta", Translated, "text"),
    entry(StreamDelta, "input_json_delta", Translated, "tool arguments"),
    entry(StreamDelta, "thinking_delta", Translated, "reasoning text"),
    entry(StreamDelta, "signature_delta", Translated, "the gateway's signature"),
    entry(StreamDelta, "citations_delta", Ignored, "upstreams return no citations"),
    entry(StreamDelta, "compaction_delta", Ignored, "only Anthropic's own service compacts"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{render_markdown, Class};
    use std::collections::HashSet;

    #[test]
    fn rows_are_unique_per_group_and_name() {
        let mut seen = HashSet::new();
        for e in TABLE {
            assert!(
                seen.insert((e.group, e.name)),
                "duplicate row {:?} {}",
                e.group,
                e.name
            );
        }
    }

    #[test]
    fn every_row_explains_its_handling() {
        for e in TABLE {
            assert!(!e.handling.is_empty(), "{} has no handling", e.name);
        }
    }

    #[test]
    fn safeguards_is_rejected_on_purpose() {
        let row = TABLE.iter().find(|e| e.name == "safeguards").unwrap();
        assert_eq!(row.class, Class::Rejected);
    }

    #[test]
    fn the_appendix_request_fields_are_all_classed() {
        let request: Vec<&str> = TABLE
            .iter()
            .filter(|e| e.group == Request)
            .map(|e| e.name)
            .collect();
        for name in [
            "model",
            "max_tokens",
            "messages",
            "system",
            "stop_sequences",
            "stream",
            "temperature",
            "top_p",
            "top_k",
            "tools",
            "tool_choice",
            "thinking",
            "output_config",
            "cache_control",
            "metadata",
            "service_tier",
            "inference_geo",
            "diagnostics",
            "speed",
            "user_profile_id",
            "workspace_id",
            "context_management",
            "compaction",
            "fallbacks",
            "fallback_credit_token",
            "container",
            "safeguards",
            "mcp_servers",
        ] {
            assert!(request.contains(&name), "missing request field {name}");
        }
    }

    #[test]
    fn every_server_tool_result_block_is_rejected() {
        for e in TABLE
            .iter()
            .filter(|e| e.group == ContentBlock && e.name.ends_with("_tool_result"))
        {
            assert_eq!(e.class, Rejected, "{}", e.name);
        }
    }

    #[test]
    fn the_generated_markdown_lists_every_row() {
        let md = render_markdown(TABLE);
        for e in TABLE {
            assert!(
                md.contains(&format!("| `{}` | {} |", e.name, e.class.as_str())),
                "{}",
                e.name
            );
        }
    }
}
