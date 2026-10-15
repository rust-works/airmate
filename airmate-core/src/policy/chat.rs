//! The OpenAI Chat Completions inbound field policy, as of 2026-10-14.
//!
//! `airmate fields --inbound chat` prints this table. Adding a translation is a row here plus its
//! handling in the parser, in the same change.

use super::{
    entry,
    Class::{Ignored, Rejected, Translated},
    Entry,
    Group::{
        BlockField, ContentBlock, MessageKey, Request, StopReason, ThinkingField, ToolChoiceField,
        ToolField,
    },
};

const NO_EQUIVALENT: &str = "no upstream equivalent";
const LEGACY: &str = "deprecated by OpenAI; use `tools`";
const SERVER_SIDE: &str = "server-side features have no upstream equivalent";

/// The Chat Completions inbound policy table.
pub const TABLE: &[Entry] = &[
    // Request fields
    entry(Request, "model", Translated, "mapped to an upstream model"),
    entry(Request, "messages", Translated, "the conversation"),
    entry(
        Request,
        "max_tokens",
        Translated,
        "the output token limit (`max_completion_tokens` wins when both are set)",
    ),
    entry(
        Request,
        "max_completion_tokens",
        Translated,
        "the output token limit",
    ),
    entry(
        Request,
        "temperature",
        Translated,
        "dropped (with a warning) for GPT-5/o-series models",
    ),
    entry(
        Request,
        "top_p",
        Translated,
        "dropped (with a warning) for GPT-5/o-series models",
    ),
    entry(
        Request,
        "top_k",
        Translated,
        "an extension some servers take; sent to Fireworks and generic upstreams",
    ),
    entry(
        Request,
        "stream",
        Translated,
        "upstream streaming, re-framed as Chat Completions chunks",
    ),
    entry(
        Request,
        "stream_options",
        Translated,
        "`include_usage` adds a final usage chunk",
    ),
    entry(Request, "stop", Translated, "a string or up to 4 strings"),
    entry(Request, "tools", Translated, "function tools"),
    entry(
        Request,
        "tool_choice",
        Translated,
        "`none`, `auto`, `required` or a named function",
    ),
    entry(
        Request,
        "parallel_tool_calls",
        Translated,
        "`false` disables parallel calls",
    ),
    entry(
        Request,
        "response_format",
        Translated,
        "`text`, `json_object` or `json_schema`",
    ),
    entry(
        Request,
        "reasoning_effort",
        Translated,
        "the reasoning effort, clamped to what the target accepts",
    ),
    entry(
        Request,
        "thinking",
        Translated,
        "an extension (Anthropic- and Fireworks-style) for reasoning on or off with a budget",
    ),
    entry(
        Request,
        "seed",
        Translated,
        "sent where the upstream takes one",
    ),
    entry(
        Request,
        "frequency_penalty",
        Translated,
        "sent where the upstream takes one",
    ),
    entry(
        Request,
        "presence_penalty",
        Translated,
        "sent where the upstream takes one",
    ),
    entry(
        Request,
        "prompt_cache_key",
        Translated,
        "sent where the upstream takes one",
    ),
    entry(
        Request,
        "n",
        Translated,
        "only `1`; more than one choice is rejected",
    ),
    entry(Request, "user", Ignored, NO_EQUIVALENT),
    entry(Request, "metadata", Ignored, NO_EQUIVALENT),
    entry(Request, "store", Ignored, "airmate stores nothing"),
    entry(Request, "service_tier", Ignored, NO_EQUIVALENT),
    entry(Request, "safety_identifier", Ignored, NO_EQUIVALENT),
    entry(Request, "verbosity", Ignored, NO_EQUIVALENT),
    entry(
        Request,
        "prediction",
        Ignored,
        "a latency hint; the answer is the same",
    ),
    entry(
        Request,
        "logit_bias",
        Rejected,
        "changes sampling and has no upstream equivalent",
    ),
    entry(
        Request,
        "logprobs",
        Rejected,
        "log probabilities cannot be produced",
    ),
    entry(
        Request,
        "top_logprobs",
        Rejected,
        "log probabilities cannot be produced",
    ),
    entry(
        Request,
        "modalities",
        Rejected,
        "audio output is not supported",
    ),
    entry(Request, "audio", Rejected, "audio output is not supported"),
    entry(Request, "web_search_options", Rejected, SERVER_SIDE),
    entry(Request, "functions", Rejected, LEGACY),
    entry(Request, "function_call", Rejected, LEGACY),
    // Message keys
    entry(
        MessageKey,
        "role",
        Translated,
        "`system`, `developer`, `user`, `assistant` or `tool`",
    ),
    entry(
        MessageKey,
        "content",
        Translated,
        "a string or a list of content parts",
    ),
    entry(
        MessageKey,
        "tool_calls",
        Translated,
        "assistant function calls",
    ),
    entry(
        MessageKey,
        "tool_call_id",
        Translated,
        "the call a tool message answers",
    ),
    entry(
        MessageKey,
        "refusal",
        Translated,
        "an assistant refusal, kept as text",
    ),
    entry(MessageKey, "name", Ignored, NO_EQUIVALENT),
    entry(
        MessageKey,
        "annotations",
        Ignored,
        "citations are not produced upstream",
    ),
    entry(
        MessageKey,
        "reasoning_content",
        Ignored,
        "reasoning is only replayed when its signature verifies; Chat Completions carries none",
    ),
    entry(
        MessageKey,
        "reasoning",
        Ignored,
        "reasoning is only replayed when its signature verifies; Chat Completions carries none",
    ),
    entry(MessageKey, "function_call", Rejected, LEGACY),
    entry(MessageKey, "audio", Rejected, "audio is not supported"),
    // Content parts
    entry(ContentBlock, "text", Translated, "text"),
    entry(
        ContentBlock,
        "image_url",
        Translated,
        "an image, as a data URI or a URL passed to the upstream",
    ),
    entry(
        ContentBlock,
        "refusal",
        Translated,
        "an assistant refusal, kept as text",
    ),
    entry(
        ContentBlock,
        "input_audio",
        Rejected,
        "audio input is not supported",
    ),
    entry(
        ContentBlock,
        "file",
        Rejected,
        "file inputs are not supported",
    ),
    entry(BlockField, "text.text", Translated, "the text"),
    entry(BlockField, "image_url.image_url", Translated, "the image"),
    entry(
        BlockField,
        "image_url.image_url.url",
        Translated,
        "the image URL or data URI",
    ),
    entry(
        BlockField,
        "image_url.image_url.detail",
        Ignored,
        "no upstream equivalent",
    ),
    entry(
        BlockField,
        "refusal.refusal",
        Translated,
        "the refusal text",
    ),
    // Tool fields
    entry(
        ToolField,
        "type",
        Translated,
        "`function`; other tool types are rejected",
    ),
    entry(ToolField, "function", Translated, "the function definition"),
    entry(
        ToolField,
        "function.name",
        Translated,
        "names over 64 characters are shortened and mapped back",
    ),
    entry(
        ToolField,
        "function.description",
        Translated,
        "the description",
    ),
    entry(
        ToolField,
        "function.parameters",
        Translated,
        "the JSON Schema of the arguments",
    ),
    entry(
        ToolField,
        "function.strict",
        Translated,
        "strict schema adherence where the upstream supports it",
    ),
    entry(ToolChoiceField, "type", Translated, "`function`"),
    entry(
        ToolChoiceField,
        "function",
        Translated,
        "the named function",
    ),
    entry(
        ToolChoiceField,
        "function.name",
        Translated,
        "the function to call",
    ),
    // Reasoning extension
    entry(
        ThinkingField,
        "type",
        Translated,
        "`enabled`, `adaptive` or `disabled`",
    ),
    entry(
        ThinkingField,
        "budget_tokens",
        Translated,
        "the thinking budget",
    ),
    // finish_reason values produced in replies
    entry(StopReason, "stop", Translated, "`end_turn`"),
    entry(StopReason, "length", Translated, "`max_tokens`"),
    entry(StopReason, "tool_calls", Translated, "`tool_use`"),
    entry(StopReason, "content_filter", Translated, "`refusal`"),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{render_markdown, Class};
    use std::collections::HashSet;

    #[test]
    fn rows_are_unique_and_explained() {
        let mut seen = HashSet::new();
        for e in TABLE {
            assert!(
                seen.insert((e.group, e.name)),
                "duplicate {:?} {}",
                e.group,
                e.name
            );
            assert!(!e.handling.is_empty());
        }
    }

    #[test]
    fn behaviour_changing_fields_are_rejected_not_dropped() {
        for name in [
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "modalities",
            "audio",
            "web_search_options",
            "functions",
            "function_call",
        ] {
            let row = TABLE
                .iter()
                .find(|e| e.group == Request && e.name == name)
                .expect(name);
            assert_eq!(row.class, Class::Rejected, "{name}");
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
