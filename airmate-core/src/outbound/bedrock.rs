//! The AWS Bedrock Converse outbound adapter: request building.
//!
//! Facts here are as documented for the Bedrock Runtime `Converse` and `ConverseStream` APIs on
//! 2026-10-14 and have not been verified against a live endpoint.

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Map, Value as Json};

use super::chat::{effective_effort, reject};
use super::sigv4::encode_segment;
use super::{Target, UpstreamRequest};
use crate::ir::{
    DocumentSource, Effort, ImageSource, IrRequest, Part, ReasoningMode, ResponseFormat,
    ToolChoiceKind, ToolResult, Turn,
};
use crate::policy::{Policy, PolicyError};
use crate::toolname::ToolNames;

/// The request path for `model`.
#[must_use]
pub fn path(model: &str, stream: bool) -> String {
    format!(
        "/model/{}/{}",
        encode_segment(model),
        if stream {
            "converse-stream"
        } else {
            "converse"
        }
    )
}

fn is_anthropic(model: &str) -> bool {
    model.starts_with("anthropic.") || model.contains(".anthropic.")
}

/// The thinking budget a level maps to for Claude models.
fn budget_for(e: Effort) -> u32 {
    match e {
        Effort::Low => 1_024,
        Effort::Medium => 4_096,
        Effort::High => 16_000,
        Effort::XHigh => 32_000,
        Effort::Max => 64_000,
    }
}

/// Builds a Converse request for `target`.
///
/// # Errors
///
/// A [`PolicyError`] for something Converse cannot take and that must not be dropped silently:
/// structured output, image URLs, unknown image or document types, or a tool result that answers
/// no call.
pub fn build(req: &IrRequest, target: &Target) -> Result<UpstreamRequest, PolicyError> {
    let mut ignored = Policy::collector();
    let mut names = ToolNames::new();
    if req.response_format != ResponseFormat::Text {
        return Err(reject(
            "response_format",
            "structured output is not supported for Bedrock Converse upstreams",
        ));
    }
    let mut body = Map::new();

    let system: Vec<&str> = req.system.iter().map(|s| s.text.as_str()).collect();
    if req.system.iter().any(|s| s.after_turns > 0) {
        ignored.ignore(
            "system_position",
            "Converse takes system prompts only at the start; later ones were moved there",
        );
    }
    if !system.is_empty() {
        body.insert("system".into(), json!([{"text": system.join("\n\n")}]));
    }
    body.insert(
        "messages".into(),
        Json::Array(messages(req, target, &mut names, &mut ignored)?),
    );

    let limit = target
        .max_output_tokens_cap
        .map_or(req.max_output_tokens, |c| {
            if req.max_output_tokens == 0 {
                c
            } else {
                c.min(req.max_output_tokens)
            }
        });
    let mut inference = Map::new();
    if limit > 0 {
        inference.insert("maxTokens".into(), json!(limit));
    }
    if let Some(t) = req.sampling.temperature {
        inference.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.sampling.top_p {
        inference.insert("topP".into(), json!(p));
    }
    if !req.stop.is_empty() {
        inference.insert("stopSequences".into(), json!(req.stop));
    }
    if !inference.is_empty() {
        body.insert("inferenceConfig".into(), Json::Object(inference));
    }
    for (set, name) in [
        (req.sampling.seed.is_some(), "seed"),
        (
            req.sampling.frequency_penalty.is_some(),
            "frequency_penalty",
        ),
        (req.sampling.presence_penalty.is_some(), "presence_penalty"),
    ] {
        if set {
            ignored.ignore(name, "Converse has no such setting");
        }
    }

    // Tools.
    if !req.tools.is_empty() {
        if req.tool_choice.kind == ToolChoiceKind::None {
            // Converse has no way to forbid calls while tools are defined, so they are withheld.
            ignored.ignore(
                "tools",
                "tool choice `none` is honoured by not sending the tools",
            );
        } else {
            let tools: Vec<Json> = req
                .tools
                .iter()
                .map(|t| {
                    let mut spec = Map::new();
                    spec.insert("name".into(), json!(names.shorten(&t.name)));
                    if let Some(d) = &t.description {
                        spec.insert("description".into(), json!(d));
                    }
                    spec.insert("inputSchema".into(), json!({"json": t.input_schema}));
                    json!({"toolSpec": Json::Object(spec)})
                })
                .collect();
            let mut cfg = Map::new();
            cfg.insert("tools".into(), Json::Array(tools));
            match &req.tool_choice.kind {
                ToolChoiceKind::Required => {
                    cfg.insert("toolChoice".into(), json!({"any": {}}));
                }
                ToolChoiceKind::Named(n) => {
                    cfg.insert(
                        "toolChoice".into(),
                        json!({"tool": {"name": names.shorten(n)}}),
                    );
                }
                ToolChoiceKind::Auto | ToolChoiceKind::None => {}
            }
            body.insert("toolConfig".into(), Json::Object(cfg));
            if !req.tool_choice.parallel {
                ignored.ignore(
                    "parallel_tool_calls",
                    "Converse cannot limit a turn to one tool call",
                );
            }
        }
    }

    // Model-specific fields.
    let mut extra = Map::new();
    if let Some(k) = req.sampling.top_k {
        if is_anthropic(&target.model) {
            extra.insert("top_k".into(), json!(k));
        } else {
            ignored.ignore("top_k", "only Claude models on Bedrock take top_k");
        }
    }
    if let Some(t) = thinking(req, target, limit, &mut ignored) {
        extra.insert("thinking".into(), t);
    }
    if !extra.is_empty() {
        body.insert("additionalModelRequestFields".into(), Json::Object(extra));
    }
    if req.cache_hints {
        ignored.ignore(
            "cache_control",
            "prompt caching on Bedrock needs cache points airmate does not place",
        );
    }

    Ok(UpstreamRequest {
        path: path(&target.model, req.stream),
        body: serde_json::to_vec(&Json::Object(body)).expect("a JSON value serialises"),
        stream: req.stream,
        tool_names: names,
        ignored: ignored.into_ignored(),
    })
}

fn thinking(req: &IrRequest, target: &Target, limit: u32, ignored: &mut Policy) -> Option<Json> {
    let budget = match req.reasoning.mode {
        ReasoningMode::On { budget_tokens } => budget_tokens
            .or_else(|| effective_effort(req).map(budget_for))
            .or(Some(4_096)),
        ReasoningMode::Adaptive => Some(effective_effort(req).map_or(4_096, budget_for)),
        ReasoningMode::Off | ReasoningMode::Unspecified => None,
    }?;
    if !is_anthropic(&target.model) {
        ignored.ignore(
            "thinking",
            "only Claude models on Bedrock take a thinking budget",
        );
        return None;
    }
    let budget = budget.max(1_024);
    // The budget has to leave room for the answer.
    if limit > 0 && budget >= limit {
        if limit > 1_024 {
            return Some(json!({"type": "enabled", "budget_tokens": limit - 1}));
        }
        ignored.ignore(
            "thinking",
            "the output limit is too small to leave room for a thinking budget",
        );
        return None;
    }
    Some(json!({"type": "enabled", "budget_tokens": budget}))
}

// ---- messages -------------------------------------------------------------------------------

fn push(out: &mut Vec<Json>, role: &str, mut content: Vec<Json>) {
    if content.is_empty() {
        return;
    }
    // Converse wants roles to alternate, so adjacent turns of one role share a message.
    if let Some(last) = out.last_mut() {
        if last["role"] == role {
            if let Some(existing) = last["content"].as_array_mut() {
                existing.append(&mut content);
                return;
            }
        }
    }
    out.push(json!({"role": role, "content": content}));
}

fn messages(
    req: &IrRequest,
    target: &Target,
    names: &mut ToolNames,
    ignored: &mut Policy,
) -> Result<Vec<Json>, PolicyError> {
    let mut out = Vec::new();
    let mut known_calls: HashMap<String, ()> = HashMap::new();
    for turn in &req.turns {
        match turn {
            Turn::User(parts) => push(&mut out, "user", user_content(parts)?),
            Turn::Assistant(parts) => {
                let c = assistant_content(parts, target, names, ignored, &mut known_calls);
                push(&mut out, "assistant", c);
            }
            Turn::ToolResults(results) => {
                let mut c = Vec::new();
                for r in results {
                    c.push(tool_result(r, &known_calls)?);
                }
                push(&mut out, "user", c);
            }
        }
    }
    Ok(out)
}

fn image_block(src: &ImageSource) -> Result<Json, PolicyError> {
    match src {
        ImageSource::Base64 { media_type, data } => {
            let format = match media_type.as_str() {
                "image/png" => "png",
                "image/jpeg" | "image/jpg" => "jpeg",
                "image/gif" => "gif",
                "image/webp" => "webp",
                other => {
                    return Err(reject(
                        "image",
                        format!("Bedrock takes png, jpeg, gif and webp images, not `{other}`"),
                    ))
                }
            };
            Ok(json!({"image": {"format": format, "source": {"bytes": data}}}))
        }
        ImageSource::Url(_) => Err(reject(
            "image",
            "Bedrock needs image bytes: send the image as base64 (airmate never fetches URLs)",
        )),
    }
}

fn user_content(parts: &[Part]) -> Result<Vec<Json>, PolicyError> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            Part::Text(t) if !t.is_empty() => out.push(json!({"text": t})),
            Part::Image(i) => out.push(image_block(i)?),
            Part::Document(DocumentSource::Text(t)) if !t.is_empty() => {
                out.push(json!({"text": t}))
            }
            Part::Document(DocumentSource::Pdf(data)) => {
                // The service wants bytes; the IR already holds base64.
                out.push(json!({"document": {"format": "pdf", "name": "document", "source": {"bytes": data}}}));
            }
            Part::Document(DocumentSource::Url(_) | DocumentSource::File(_)) => {
                return Err(reject(
                    "document",
                    "Bedrock takes documents as text or a base64 PDF, not by URL or file id",
                ))
            }
            _ => {}
        }
    }
    Ok(out)
}

fn assistant_content(
    parts: &[Part],
    target: &Target,
    names: &mut ToolNames,
    ignored: &mut Policy,
    known: &mut HashMap<String, ()>,
) -> Vec<Json> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            Part::Text(t) if !t.is_empty() => out.push(json!({"text": t})),
            Part::Reasoning {
                text,
                origin,
                opaque,
                ..
            } => {
                if origin.as_ref() != Some(&target.origin()) {
                    ignored.ignore(
                        "thinking_foreign_origin",
                        "the reasoning was produced by a different upstream",
                    );
                } else if let Some(sig) = opaque
                    .as_ref()
                    .and_then(|o| String::from_utf8(o.clone()).ok())
                {
                    out.push(json!({"reasoningContent": {"reasoningText": {"text": text, "signature": sig}}}));
                }
            }
            Part::ToolCall { id, name, input } => {
                known.insert(id.clone(), ());
                out.push(json!({"toolUse": {"toolUseId": id, "name": names.shorten(name), "input": input}}));
            }
            _ => {}
        }
    }
    out
}

fn tool_result(r: &ToolResult, known: &HashMap<String, ()>) -> Result<Json, PolicyError> {
    if !known.contains_key(&r.call_id) {
        return Err(reject(
            "tool_result",
            format!(
                "the tool result for `{}` answers no tool call in the conversation",
                r.call_id
            ),
        ));
    }
    let mut content = Vec::new();
    for p in &r.content {
        match p {
            Part::Text(t) | Part::Document(DocumentSource::Text(t)) if !t.is_empty() => {
                content.push(json!({"text": t}));
            }
            Part::Image(i) => content.push(image_block(i)?),
            _ => {}
        }
    }
    if content.is_empty() {
        // A tool result may not be empty.
        content.push(json!({"text": "(no output)"}));
    }
    Ok(json!({"toolResult": {
        "toolUseId": r.call_id,
        "content": content,
        "status": if r.is_error { "error" } else { "success" },
    }}))
}

/// Whether `bytes` is valid standard base64 (used to vet payloads before they are sent).
#[must_use]
pub fn is_base64(s: &str) -> bool {
    STANDARD.decode(s).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;
    use crate::ir::{
        Origin, Protocol, Reasoning, ReasoningDisplay, Sampling, SystemPart, ToolChoice, ToolDef,
    };

    const CLAUDE: &str = "anthropic.claude-3-5-sonnet-20241022-v2:0";

    fn target(model: &str) -> Target {
        Target::new("b", Protocol::Bedrock, Flavor::Generic, model)
    }

    fn ir() -> IrRequest {
        let mut r = IrRequest::new("claude-x", Protocol::Anthropic);
        r.max_output_tokens = 4096;
        r.turns.push(Turn::User(vec![Part::Text("hi".into())]));
        r
    }

    fn built(r: &IrRequest, model: &str) -> Json {
        serde_json::from_slice(&build(r, &target(model)).unwrap().body).unwrap()
    }

    #[test]
    fn the_path_encodes_the_model_id_and_picks_the_mode() {
        assert_eq!(
            path(CLAUDE, false),
            "/model/anthropic.claude-3-5-sonnet-20241022-v2%3A0/converse"
        );
        assert_eq!(
            path("amazon.nova-pro-v1:0", true),
            "/model/amazon.nova-pro-v1%3A0/converse-stream"
        );
        let mut r = ir();
        r.stream = true;
        assert!(build(&r, &target(CLAUDE))
            .unwrap()
            .path
            .ends_with("/converse-stream"));
    }

    #[test]
    fn a_plain_request_has_system_messages_and_inference_config() {
        let mut r = ir();
        r.system.push(SystemPart {
            text: "be brief".into(),
            after_turns: 0,
            effort: None,
        });
        r.sampling = Sampling {
            temperature: Some(0.2),
            top_p: Some(0.9),
            ..Sampling::default()
        };
        r.stop = vec!["END".into()];
        let b = built(&r, CLAUDE);
        assert_eq!(b["system"], json!([{"text": "be brief"}]));
        assert_eq!(
            b["messages"],
            json!([{"role": "user", "content": [{"text": "hi"}]}])
        );
        assert_eq!(
            b["inferenceConfig"],
            json!({"maxTokens": 4096, "temperature": 0.2, "topP": 0.9, "stopSequences": ["END"]})
        );
        assert!(b.get("toolConfig").is_none() && b.get("additionalModelRequestFields").is_none());
    }

    #[test]
    fn tools_calls_and_results_use_tool_use_ids_and_merge_into_alternating_roles() {
        let mut r = ir();
        r.tools.push(ToolDef {
            name: "get_weather".into(),
            description: Some("w".into()),
            input_schema: json!({"type": "object"}),
            strict: None,
        });
        r.turns.push(Turn::Assistant(vec![
            Part::Text("checking".into()),
            Part::ToolCall {
                id: "tu_1".into(),
                name: "get_weather".into(),
                input: json!({"city": "Paris"}),
            },
        ]));
        r.turns.push(Turn::ToolResults(vec![ToolResult {
            call_id: "tu_1".into(),
            content: vec![Part::Text("sunny".into())],
            is_error: false,
        }]));
        r.turns.push(Turn::User(vec![Part::Text("thanks".into())]));
        r.tool_choice = ToolChoice {
            kind: ToolChoiceKind::Required,
            parallel: true,
        };
        let b = built(&r, CLAUDE);
        assert_eq!(
            b["toolConfig"]["tools"][0]["toolSpec"]["inputSchema"],
            json!({"json": {"type": "object"}})
        );
        assert_eq!(b["toolConfig"]["toolChoice"], json!({"any": {}}));
        assert_eq!(
            b["messages"][1],
            json!({"role": "assistant", "content": [{"text": "checking"}, {"toolUse": {"toolUseId": "tu_1", "name": "get_weather", "input": {"city": "Paris"}}}]})
        );
        assert_eq!(
            b["messages"][2],
            json!({"role": "user", "content": [{"toolResult": {"toolUseId": "tu_1", "content": [{"text": "sunny"}], "status": "success"}}, {"text": "thanks"}]})
        );
    }

    #[test]
    fn named_tool_choice_and_none() {
        let mut r = ir();
        r.tools.push(ToolDef {
            name: "t".into(),
            description: None,
            input_schema: json!({"type": "object"}),
            strict: None,
        });
        r.tool_choice = ToolChoice {
            kind: ToolChoiceKind::Named("t".into()),
            parallel: true,
        };
        assert_eq!(
            built(&r, CLAUDE)["toolConfig"]["toolChoice"],
            json!({"tool": {"name": "t"}})
        );
        r.tool_choice = ToolChoice {
            kind: ToolChoiceKind::None,
            parallel: true,
        };
        let u = build(&r, &target(CLAUDE)).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert!(
            b.get("toolConfig").is_none(),
            "tools are withheld when calls are forbidden"
        );
        assert!(u.ignored.iter().any(|i| i.name == "tools"));
    }

    #[test]
    fn tool_errors_and_empty_results_are_well_formed() {
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![Part::ToolCall {
            id: "c".into(),
            name: "t".into(),
            input: json!({}),
        }]));
        r.turns.push(Turn::ToolResults(vec![ToolResult {
            call_id: "c".into(),
            content: vec![],
            is_error: true,
        }]));
        let b = built(&r, CLAUDE);
        assert_eq!(
            b["messages"][2]["content"][0]["toolResult"],
            json!({"toolUseId": "c", "content": [{"text": "(no output)"}], "status": "error"})
        );
        let mut ghost = ir();
        ghost.turns.push(Turn::ToolResults(vec![ToolResult {
            call_id: "ghost".into(),
            content: vec![],
            is_error: false,
        }]));
        assert_eq!(
            build(&ghost, &target(CLAUDE)).unwrap_err().field,
            "tool_result"
        );
    }

    #[test]
    fn thinking_is_a_budget_on_claude_and_ignored_elsewhere() {
        let mut r = ir();
        r.reasoning = Reasoning {
            mode: ReasoningMode::On {
                budget_tokens: Some(2_000),
            },
            ..Reasoning::default()
        };
        assert_eq!(
            built(&r, CLAUDE)["additionalModelRequestFields"]["thinking"],
            json!({"type": "enabled", "budget_tokens": 2000})
        );
        r.reasoning = Reasoning {
            mode: ReasoningMode::On {
                budget_tokens: None,
            },
            effort: Some(Effort::High),
            ..Reasoning::default()
        };
        r.max_output_tokens = 32_000;
        assert_eq!(
            built(&r, CLAUDE)["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            16_000
        );
        r.max_output_tokens = 8_000;
        assert_eq!(
            built(&r, CLAUDE)["additionalModelRequestFields"]["thinking"]["budget_tokens"],
            7_999,
            "leaves room for the answer"
        );
        r.max_output_tokens = 1_000;
        let u = build(&r, &target(CLAUDE)).unwrap();
        assert!(u.ignored.iter().any(|i| i.name == "thinking"));
        r.max_output_tokens = 4096;
        let nova = build(&r, &target("amazon.nova-pro-v1:0")).unwrap();
        assert!(nova.ignored.iter().any(|i| i.name == "thinking"));
        assert!(!String::from_utf8_lossy(&nova.body).contains("thinking"));
    }

    #[test]
    fn top_k_goes_only_to_claude() {
        let mut r = ir();
        r.sampling.top_k = Some(40);
        assert_eq!(
            built(&r, CLAUDE)["additionalModelRequestFields"]["top_k"],
            40
        );
        let u = build(&r, &target("meta.llama3-70b-instruct-v1:0")).unwrap();
        assert!(u.ignored.iter().any(|i| i.name == "top_k"));
    }

    #[test]
    fn structured_output_is_refused_not_dropped() {
        let mut r = ir();
        r.response_format = ResponseFormat::JsonObject;
        assert_eq!(
            build(&r, &target(CLAUDE)).unwrap_err().field,
            "response_format"
        );
    }

    #[test]
    fn images_must_be_bytes_in_a_known_format_and_pdfs_are_documents() {
        let mut r = ir();
        r.turns[0] = Turn::User(vec![
            Part::Image(ImageSource::Base64 {
                media_type: "image/jpeg".into(),
                data: "AAAA".into(),
            }),
            Part::Document(DocumentSource::Pdf("JVBER".into())),
        ]);
        let b = built(&r, CLAUDE);
        assert_eq!(
            b["messages"][0]["content"][0],
            json!({"image": {"format": "jpeg", "source": {"bytes": "AAAA"}}})
        );
        assert_eq!(b["messages"][0]["content"][1]["document"]["format"], "pdf");
        r.turns[0] = Turn::User(vec![Part::Image(ImageSource::Url(
            "https://x/y.png".into(),
        ))]);
        assert_eq!(build(&r, &target(CLAUDE)).unwrap_err().field, "image");
        r.turns[0] = Turn::User(vec![Part::Image(ImageSource::Base64 {
            media_type: "image/tiff".into(),
            data: "AA".into(),
        })]);
        assert!(build(&r, &target(CLAUDE))
            .unwrap_err()
            .message
            .contains("tiff"));
        r.turns[0] = Turn::User(vec![Part::Document(DocumentSource::File("file_1".into()))]);
        assert_eq!(build(&r, &target(CLAUDE)).unwrap_err().field, "document");
    }

    #[test]
    fn signed_reasoning_is_replayed_for_its_own_upstream_only() {
        let t = target(CLAUDE);
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![
            Part::Reasoning {
                text: "thought".into(),
                display: ReasoningDisplay::Summarized,
                origin: Some(t.origin()),
                opaque: Some(b"sig123".to_vec()),
            },
            Part::Reasoning {
                text: "other".into(),
                display: ReasoningDisplay::Summarized,
                origin: Some(Origin {
                    upstream: "x".into(),
                    protocol: Protocol::Chat,
                }),
                opaque: None,
            },
            Part::Text("answer".into()),
        ]));
        let u = build(&r, &t).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert_eq!(
            b["messages"][1]["content"][0],
            json!({"reasoningContent": {"reasoningText": {"text": "thought", "signature": "sig123"}}})
        );
        assert_eq!(b["messages"][1]["content"][1], json!({"text": "answer"}));
        assert!(u
            .ignored
            .iter()
            .any(|i| i.name == "thinking_foreign_origin"));
    }

    #[test]
    fn unsupported_settings_are_reported_and_late_system_text_is_moved() {
        let mut r = ir();
        r.sampling = Sampling {
            seed: Some(1),
            frequency_penalty: Some(0.5),
            presence_penalty: Some(0.5),
            ..Sampling::default()
        };
        r.cache_hints = true;
        r.system.push(SystemPart {
            text: "later".into(),
            after_turns: 1,
            effort: None,
        });
        let u = build(&r, &target(CLAUDE)).unwrap();
        let mut names: Vec<&str> = u.ignored.iter().map(|i| i.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "cache_control",
                "frequency_penalty",
                "presence_penalty",
                "seed",
                "system_position"
            ]
        );
        assert!(is_base64("AAAA") && !is_base64("not base64!"));
    }
}
