//! The OpenAI Responses inbound field policy, as of 2026-10-14.
//!
//! `airmate fields --inbound responses` prints this table. Adding a translation is a row here plus
//! its handling in the parser, in the same change.

use super::{
    entry,
    Class::{Ignored, Rejected, Translated},
    Entry,
    Group::{
        BlockField, ContentPart, InputItem, Request, ThinkingField, ToolChoiceField, ToolField,
    },
};

const NO_EQUIVALENT: &str = "no upstream equivalent";
const SERVER_SIDE: &str = "server-side tools and state have no upstream equivalent";
const NO_STATE: &str = "airmate keeps no server-side conversation state; send the full history";
const ACCEPTED_ID: &str = "accepted; the upstream assigns its own ids";

/// The Responses inbound policy table.
pub const TABLE: &[Entry] = &[
    // Request fields
    entry(Request, "model", Translated, "mapped to an upstream model"),
    entry(Request, "input", Translated, "a string or a list of items"),
    entry(Request, "instructions", Translated, "a leading system message"),
    entry(Request, "max_output_tokens", Translated, "the output token limit"),
    entry(Request, "temperature", Translated, "dropped (with a warning) for GPT-5/o-series models"),
    entry(Request, "top_p", Translated, "dropped (with a warning) for GPT-5/o-series models"),
    entry(Request, "stream", Translated, "upstream streaming, re-framed as Responses events"),
    entry(Request, "tools", Translated, "function tools"),
    entry(Request, "tool_choice", Translated, "`none`, `auto`, `required` or a named function"),
    entry(Request, "parallel_tool_calls", Translated, "`false` disables parallel calls"),
    entry(Request, "reasoning", Translated, "effort and summary"),
    entry(Request, "text", Translated, "`format` selects text, json_object or json_schema output"),
    entry(Request, "include", Translated, "`reasoning.encrypted_content` is always honoured; other values are ignored"),
    entry(Request, "prompt_cache_key", Translated, "sent where the upstream takes one"),
    entry(Request, "store", Ignored, "airmate stores nothing"),
    entry(Request, "metadata", Ignored, NO_EQUIVALENT),
    entry(Request, "user", Ignored, NO_EQUIVALENT),
    entry(Request, "safety_identifier", Ignored, NO_EQUIVALENT),
    entry(Request, "service_tier", Ignored, NO_EQUIVALENT),
    entry(Request, "truncation", Ignored, "the history is sent whole; an overflow returns `prompt is too long`"),
    entry(Request, "max_tool_calls", Ignored, NO_EQUIVALENT),
    entry(Request, "stream_options", Ignored, NO_EQUIVALENT),
    entry(Request, "previous_response_id", Rejected, NO_STATE),
    entry(Request, "conversation", Rejected, NO_STATE),
    entry(Request, "background", Rejected, "background responses need server-side state"),
    entry(Request, "prompt", Rejected, "stored prompt templates need server-side state"),
    entry(Request, "top_logprobs", Rejected, "log probabilities cannot be produced"),
    // Input item types
    entry(InputItem, "message", Translated, "a conversation message"),
    entry(InputItem, "function_call", Translated, "an assistant function call"),
    entry(InputItem, "function_call_output", Translated, "a tool result"),
    entry(InputItem, "reasoning", Translated, "replayed as upstream reasoning when its signature verifies and the origin matches, else dropped"),
    entry(InputItem, "item_reference", Rejected, NO_STATE),
    entry(InputItem, "web_search_call", Rejected, SERVER_SIDE),
    entry(InputItem, "file_search_call", Rejected, SERVER_SIDE),
    entry(InputItem, "computer_call", Rejected, SERVER_SIDE),
    entry(InputItem, "computer_call_output", Rejected, SERVER_SIDE),
    entry(InputItem, "code_interpreter_call", Rejected, SERVER_SIDE),
    entry(InputItem, "image_generation_call", Rejected, SERVER_SIDE),
    entry(InputItem, "local_shell_call", Rejected, SERVER_SIDE),
    entry(InputItem, "local_shell_call_output", Rejected, SERVER_SIDE),
    entry(InputItem, "mcp_call", Rejected, SERVER_SIDE),
    entry(InputItem, "mcp_list_tools", Rejected, SERVER_SIDE),
    entry(InputItem, "mcp_approval_request", Rejected, SERVER_SIDE),
    entry(InputItem, "mcp_approval_response", Rejected, SERVER_SIDE),
    entry(InputItem, "custom_tool_call", Rejected, SERVER_SIDE),
    entry(InputItem, "custom_tool_call_output", Rejected, SERVER_SIDE),
    // Item keys, `<type>.<key>`
    entry(BlockField, "message.type", Translated, "`message`"),
    entry(BlockField, "message.role", Translated, "`user`, `assistant`, `system` or `developer`"),
    entry(BlockField, "message.content", Translated, "a string or a list of content parts"),
    entry(BlockField, "message.id", Translated, ACCEPTED_ID),
    entry(BlockField, "message.status", Translated, ACCEPTED_ID),
    entry(BlockField, "function_call.type", Translated, "`function_call`"),
    entry(BlockField, "function_call.call_id", Translated, "the call id"),
    entry(BlockField, "function_call.name", Translated, "names over 64 characters are shortened and mapped back"),
    entry(BlockField, "function_call.arguments", Translated, "the call arguments, as a JSON string"),
    entry(BlockField, "function_call.id", Translated, ACCEPTED_ID),
    entry(BlockField, "function_call.status", Translated, ACCEPTED_ID),
    entry(BlockField, "function_call_output.type", Translated, "`function_call_output`"),
    entry(BlockField, "function_call_output.call_id", Translated, "the id of the call this answers"),
    entry(BlockField, "function_call_output.output", Translated, "a string, or a list of text and image parts"),
    entry(BlockField, "function_call_output.id", Translated, ACCEPTED_ID),
    entry(BlockField, "function_call_output.status", Translated, ACCEPTED_ID),
    entry(BlockField, "reasoning.type", Translated, "`reasoning`"),
    entry(BlockField, "reasoning.summary", Translated, "the reasoning text"),
    entry(BlockField, "reasoning.encrypted_content", Translated, "the gateway's signature, which carries the upstream's encrypted content"),
    entry(BlockField, "reasoning.id", Translated, ACCEPTED_ID),
    entry(BlockField, "reasoning.status", Translated, ACCEPTED_ID),
    entry(BlockField, "reasoning.content", Ignored, "raw reasoning text is not replayed"),
    // Content parts
    entry(ContentPart, "input_text", Translated, "text"),
    entry(ContentPart, "output_text", Translated, "assistant text"),
    entry(ContentPart, "refusal", Translated, "an assistant refusal, kept as text"),
    entry(ContentPart, "input_image", Translated, "an image, as a data URI or a URL passed to the upstream; file ids are rejected"),
    entry(ContentPart, "input_file", Rejected, "file inputs are not supported"),
    entry(ContentPart, "input_audio", Rejected, "audio input is not supported"),
    entry(BlockField, "input_text.text", Translated, "the text"),
    entry(BlockField, "output_text.text", Translated, "the text"),
    entry(BlockField, "output_text.annotations", Ignored, "citations are not produced upstream"),
    entry(BlockField, "output_text.logprobs", Ignored, "log probabilities are not produced"),
    entry(BlockField, "refusal.refusal", Translated, "the refusal text"),
    entry(BlockField, "input_image.image_url", Translated, "the image"),
    entry(BlockField, "input_image.detail", Ignored, NO_EQUIVALENT),
    entry(BlockField, "input_image.file_id", Rejected, "file ids need server-side storage"),
    // Tool fields
    entry(ToolField, "type", Translated, "`function`; other tool types are rejected"),
    entry(ToolField, "name", Translated, "names over 64 characters are shortened and mapped back"),
    entry(ToolField, "description", Translated, "the description"),
    entry(ToolField, "parameters", Translated, "the JSON Schema of the arguments"),
    entry(ToolField, "strict", Translated, "strict schema adherence where the upstream supports it"),
    entry(ToolChoiceField, "type", Translated, "`function`"),
    entry(ToolChoiceField, "name", Translated, "the function to call"),
    // reasoning object
    entry(ThinkingField, "effort", Translated, "the reasoning effort, clamped to what the target accepts"),
    entry(ThinkingField, "summary", Translated, "asks for reasoning to be returned"),
    entry(ThinkingField, "generate_summary", Translated, "asks for reasoning to be returned (deprecated name)"),
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
    fn server_side_state_and_tools_are_rejected_not_dropped() {
        for name in [
            "previous_response_id",
            "conversation",
            "background",
            "prompt",
        ] {
            assert_eq!(
                TABLE
                    .iter()
                    .find(|e| e.group == Request && e.name == name)
                    .unwrap()
                    .class,
                Class::Rejected,
                "{name}"
            );
        }
        for e in TABLE
            .iter()
            .filter(|e| e.group == InputItem && e.name.ends_with("_call"))
        {
            if e.name != "function_call" {
                assert_eq!(e.class, Class::Rejected, "{}", e.name);
            }
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
