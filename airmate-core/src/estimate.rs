//! Token estimation.
//!
//! No OpenAI-protocol server exposes its tokenizer, so `count_tokens` is an estimate: about four
//! characters per token over everything the upstream would see, plus a fixed cost per message,
//! tool and image. Clients use the count to decide when to compact, where an estimate is enough.
//! The same estimator feeds routing's context-window filter.

use crate::ir::{DocumentSource, IrRequest, Part, ToolResult, Turn};

/// Characters per token.
pub const CHARS_PER_TOKEN: usize = 4;
/// Fixed overhead per message.
pub const TOKENS_PER_MESSAGE: u64 = 4;
/// Fixed overhead per tool definition.
pub const TOKENS_PER_TOOL: u64 = 8;
/// Cost of one image.
pub const TOKENS_PER_IMAGE: u64 = 1_600;

/// The value of the `x-airmate-token-estimate` header on estimated counts.
pub const ESTIMATE_HEADER_VALUE: &str = "heuristic";

fn text_tokens(chars: usize) -> u64 {
    chars.div_ceil(CHARS_PER_TOKEN) as u64
}

fn part_tokens(part: &Part) -> u64 {
    match part {
        Part::Text(t) => text_tokens(t.chars().count()),
        Part::Image(_) => TOKENS_PER_IMAGE,
        Part::Document(DocumentSource::Text(t)) => text_tokens(t.chars().count()),
        Part::Document(_) => 0,
        Part::ToolCall { name, input, .. } => {
            text_tokens(name.chars().count()) + text_tokens(input.to_string().chars().count())
        }
        Part::Reasoning { text, .. } => text_tokens(text.chars().count()),
    }
}

fn tool_result_tokens(r: &ToolResult) -> u64 {
    TOKENS_PER_MESSAGE + r.content.iter().map(part_tokens).sum::<u64>()
}

/// Estimates the input tokens of `req`.
#[must_use]
pub fn estimate_input_tokens(req: &IrRequest) -> u64 {
    let system: u64 = req
        .system
        .iter()
        .map(|s| TOKENS_PER_MESSAGE + text_tokens(s.text.chars().count()))
        .sum();
    let turns: u64 = req
        .turns
        .iter()
        .map(|t| match t {
            Turn::User(parts) | Turn::Assistant(parts) => {
                TOKENS_PER_MESSAGE + parts.iter().map(part_tokens).sum::<u64>()
            }
            Turn::ToolResults(results) => results.iter().map(tool_result_tokens).sum(),
        })
        .sum();
    let tools: u64 = req
        .tools
        .iter()
        .map(|t| {
            TOKENS_PER_TOOL
                + text_tokens(t.name.chars().count())
                + text_tokens(t.description.as_deref().map_or(0, |d| d.chars().count()))
                + text_tokens(t.input_schema.to_string().chars().count())
        })
        .sum();
    system + turns + tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{ImageSource, Protocol, SystemPart, ToolDef, ToolResult};
    use serde_json::json;

    fn req() -> IrRequest {
        IrRequest::new("m", Protocol::Anthropic)
    }

    #[test]
    fn an_empty_request_costs_nothing() {
        assert_eq!(estimate_input_tokens(&req()), 0);
    }

    #[test]
    fn text_costs_one_token_per_four_characters_rounded_up() {
        let mut r = req();
        r.turns
            .push(Turn::User(vec![Part::Text("12345678".into())]));
        assert_eq!(estimate_input_tokens(&r), TOKENS_PER_MESSAGE + 2);
        r.turns[0] = Turn::User(vec![Part::Text("123456789".into())]);
        assert_eq!(estimate_input_tokens(&r), TOKENS_PER_MESSAGE + 3);
    }

    #[test]
    fn characters_not_bytes_are_counted() {
        let mut r = req();
        r.turns.push(Turn::User(vec![Part::Text(
            "\u{1f600}\u{1f600}\u{1f600}\u{1f600}".into(),
        )]));
        assert_eq!(estimate_input_tokens(&r), TOKENS_PER_MESSAGE + 1);
    }

    #[test]
    fn images_tools_and_system_have_fixed_costs() {
        let mut r = req();
        r.system.push(SystemPart {
            text: "abcd".into(),
            after_turns: 0,
            effort: None,
        });
        r.turns
            .push(Turn::User(vec![Part::Image(ImageSource::Url("u".into()))]));
        r.tools.push(ToolDef {
            name: "abcd".into(),
            description: None,
            input_schema: json!({}),
            strict: None,
        });
        let want = (TOKENS_PER_MESSAGE + 1)
            + (TOKENS_PER_MESSAGE + TOKENS_PER_IMAGE)
            + (TOKENS_PER_TOOL + 1 + 1);
        assert_eq!(estimate_input_tokens(&r), want);
    }

    #[test]
    fn tool_calls_and_results_are_counted() {
        let mut r = req();
        r.turns.push(Turn::Assistant(vec![Part::ToolCall {
            id: "c".into(),
            name: "read".into(),
            input: json!({"path": "a"}),
        }]));
        r.turns.push(Turn::ToolResults(vec![ToolResult {
            call_id: "c".into(),
            content: vec![Part::Text("abcdefgh".into())],
            is_error: false,
        }]));
        assert!(estimate_input_tokens(&r) > 2 * TOKENS_PER_MESSAGE);
    }

    #[test]
    fn non_text_documents_and_reasoning_are_handled() {
        let mut r = req();
        r.turns.push(Turn::User(vec![
            Part::Document(DocumentSource::Pdf("x".into())),
            Part::Document(DocumentSource::Text("abcd".into())),
        ]));
        r.turns.push(Turn::Assistant(vec![Part::Reasoning {
            text: "abcd".into(),
            display: crate::ir::ReasoningDisplay::Summarized,
            origin: None,
            opaque: None,
        }]));
        assert_eq!(estimate_input_tokens(&r), 2 * TOKENS_PER_MESSAGE + 2);
    }
}
