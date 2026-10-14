//! The OpenAI Responses outbound adapter: request building.
//!
//! airmate sends `store: false` and asks for encrypted reasoning, so the next turn can replay the
//! reasoning item from the signature the gateway issued.

use serde_json::{json, Map, Value as Json};

use super::chat::{effective_effort, image_url, reject, text_of};
use super::{Target, UpstreamRequest};
use crate::cache::prompt_cache_key;
use crate::flavor::is_reasoning_family;
use crate::ir::{
    DocumentSource, Effort, IrRequest, Part, ReasoningMode, ResponseFormat, ToolChoiceKind,
    ToolResult, Turn,
};
use crate::policy::{Policy, PolicyError};
use crate::toolname::ToolNames;

/// The Responses path.
pub const PATH: &str = "/v1/responses";

/// Builds a Responses request for `target`.
///
/// # Errors
///
/// A [`PolicyError`] for `stop_sequences` (the Responses API has no equivalent) or a zero limit.
pub fn build(req: &IrRequest, target: &Target) -> Result<UpstreamRequest, PolicyError> {
    if !req.stop.is_empty() {
        return Err(reject(
            "stop_sequences",
            "the Responses API has no stop sequences; use a chat or anthropic upstream",
        ));
    }
    let limit = target
        .max_output_tokens_cap
        .map_or(req.max_output_tokens, |c| {
            if req.max_output_tokens == 0 {
                c
            } else {
                c.min(req.max_output_tokens)
            }
        });

    let mut ignored = Policy::collector();
    let mut names = ToolNames::new();
    let flavor = target.flavor;
    let mut body = Map::new();

    body.insert("model".into(), json!(target.model));
    body.insert("store".into(), json!(false));
    body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    body.insert("stream".into(), json!(req.stream));
    if limit > 0 {
        body.insert(
            flavor
                .token_limit_field(target.protocol, false)
                .key()
                .into(),
            json!(limit),
        );
    }

    let lead: Vec<&str> = req
        .system
        .iter()
        .filter(|s| s.after_turns == 0)
        .map(|s| s.text.as_str())
        .collect();
    if !lead.is_empty() {
        body.insert("instructions".into(), json!(lead.join("\n\n")));
    }
    body.insert(
        "input".into(),
        Json::Array(input(req, target, &mut names, &mut ignored)),
    );

    if is_reasoning_family(&target.model) {
        if req.sampling.temperature.is_some() {
            ignored.ignore(
                "temperature",
                "the model accepts only the default temperature",
            );
        }
        if req.sampling.top_p.is_some() {
            ignored.ignore("top_p", "the model accepts only the default top_p");
        }
    } else {
        if let Some(t) = req.sampling.temperature {
            body.insert("temperature".into(), json!(t));
        }
        if let Some(p) = req.sampling.top_p {
            body.insert("top_p".into(), json!(p));
        }
    }
    if req.sampling.top_k.is_some() {
        ignored.ignore("top_k", "the Responses API does not take top_k");
    }

    if !req.tools.is_empty() {
        let tools: Vec<Json> = req
            .tools
            .iter()
            .map(|t| {
                let mut f = Map::new();
                f.insert("type".into(), json!("function"));
                f.insert("name".into(), json!(names.shorten(&t.name)));
                if let Some(d) = &t.description {
                    f.insert("description".into(), json!(d));
                }
                f.insert("parameters".into(), t.input_schema.clone());
                if let Some(s) = t.strict {
                    f.insert(
                        "strict".into(),
                        json!(s && flavor.strict_structured_output()),
                    );
                }
                Json::Object(f)
            })
            .collect();
        body.insert("tools".into(), Json::Array(tools));
        match &req.tool_choice.kind {
            ToolChoiceKind::Auto => {}
            ToolChoiceKind::Required => {
                body.insert("tool_choice".into(), json!("required"));
            }
            ToolChoiceKind::None => {
                body.insert("tool_choice".into(), json!("none"));
            }
            ToolChoiceKind::Named(n) => {
                body.insert(
                    "tool_choice".into(),
                    json!({"type": "function", "name": names.shorten(n)}),
                );
            }
        }
        if !req.tool_choice.parallel {
            body.insert("parallel_tool_calls".into(), json!(false));
        }
    }

    // Reasoning: effort as for Chat, plus summaries so the client has something to show.
    let accepted = target.accepted_efforts();
    let effort = effective_effort(req);
    let level: Option<Effort> = match req.reasoning.mode {
        ReasoningMode::On { budget_tokens } => {
            effort.or_else(|| budget_tokens.map(Effort::from_budget))
        }
        ReasoningMode::Adaptive | ReasoningMode::Unspecified => effort,
        ReasoningMode::Off => None,
    }
    .and_then(|e| e.clamp_down(accepted));
    if level.is_some() || req.reasoning.wanted() {
        let mut r = Map::new();
        if let Some(l) = level {
            r.insert("effort".into(), json!(l.as_str()));
        }
        if req.reasoning.wanted() {
            r.insert("summary".into(), json!("auto"));
        }
        body.insert("reasoning".into(), Json::Object(r));
    }

    if let ResponseFormat::JsonSchema { schema, strict } = &req.response_format {
        body.insert(
            "text".into(),
            json!({"format": {
                "type": "json_schema",
                "name": "response",
                "schema": schema,
                "strict": *strict && flavor.strict_structured_output(),
            }}),
        );
    }
    if req.cache_hints && flavor.sends_prompt_cache_key() {
        body.insert("prompt_cache_key".into(), json!(prompt_cache_key(req)));
    }

    Ok(UpstreamRequest {
        path: PATH.to_owned(),
        body: serde_json::to_vec(&Json::Object(body)).expect("a JSON value serialises"),
        stream: req.stream,
        tool_names: names,
        ignored: ignored.into_ignored(),
    })
}

fn input_text_item(role: &str, text: &str) -> Json {
    json!({"type": "message", "role": role, "content": [{"type": "input_text", "text": text}]})
}

fn input(
    req: &IrRequest,
    target: &Target,
    names: &mut ToolNames,
    ignored: &mut Policy,
) -> Vec<Json> {
    let mut out = Vec::new();
    for (i, turn) in req.turns.iter().enumerate() {
        for s in req.system.iter().filter(|s| s.after_turns == i && i != 0) {
            out.push(input_text_item("developer", &s.text));
        }
        match turn {
            Turn::User(parts) => out.push(user_item(parts)),
            Turn::Assistant(parts) => assistant_items(parts, target, names, ignored, &mut out),
            Turn::ToolResults(results) => tool_result_items(results, &mut out),
        }
    }
    for s in req
        .system
        .iter()
        .filter(|s| s.after_turns >= req.turns.len() && s.after_turns != 0)
    {
        out.push(input_text_item("developer", &s.text));
    }
    out
}

fn user_item(parts: &[Part]) -> Json {
    let mut content = Vec::new();
    for p in parts {
        match p {
            Part::Text(t) | Part::Document(DocumentSource::Text(t)) => {
                content.push(json!({"type": "input_text", "text": t}));
            }
            Part::Image(i) => {
                content.push(json!({"type": "input_image", "image_url": image_url(i)}))
            }
            _ => {}
        }
    }
    json!({"type": "message", "role": "user", "content": content})
}

fn assistant_items(
    parts: &[Part],
    target: &Target,
    names: &mut ToolNames,
    ignored: &mut Policy,
    out: &mut Vec<Json>,
) {
    let mut text = String::new();
    let flush = |text: &mut String, out: &mut Vec<Json>| {
        if !text.is_empty() {
            out.push(json!({"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": std::mem::take(text)}]}));
        }
    };
    for p in parts {
        match p {
            Part::Text(t) => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(t);
            }
            Part::Reasoning {
                text: r,
                origin,
                opaque,
                ..
            } => {
                if origin.as_ref() != Some(&target.origin()) {
                    ignored.ignore(
                        "thinking_foreign_origin",
                        "the reasoning was produced by a different upstream",
                    );
                    continue;
                }
                match opaque
                    .as_ref()
                    .and_then(|b| String::from_utf8(b.clone()).ok())
                {
                    Some(enc) => {
                        flush(&mut text, out);
                        let summary = if r.is_empty() {
                            json!([])
                        } else {
                            json!([{"type": "summary_text", "text": r}])
                        };
                        out.push(json!({"type": "reasoning", "summary": summary, "encrypted_content": enc}));
                    }
                    None => ignored.ignore(
                        "thinking_not_replayed",
                        "the reasoning carries no encrypted content to replay",
                    ),
                }
            }
            Part::ToolCall { id, name, input } => {
                flush(&mut text, out);
                out.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": names.shorten(name),
                    "arguments": input.to_string(),
                }));
            }
            Part::Image(_) | Part::Document(_) => {}
        }
    }
    flush(&mut text, out);
}

fn tool_result_items(results: &[ToolResult], out: &mut Vec<Json>) {
    let mut images = Vec::new();
    for r in results {
        let mut text = text_of(&r.content);
        if r.is_error {
            text = format!("[error] {text}");
        }
        out.push(json!({"type": "function_call_output", "call_id": r.call_id, "output": text}));
        for p in &r.content {
            if let Part::Image(i) = p {
                images.push(json!({"type": "input_image", "image_url": image_url(i)}));
            }
        }
    }
    if !images.is_empty() {
        let mut content =
            vec![json!({"type": "input_text", "text": "Images returned by the tool calls above:"})];
        content.extend(images);
        out.push(json!({"type": "message", "role": "user", "content": content}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;
    use crate::ir::{
        ImageSource, Origin, Protocol, Reasoning, ReasoningDisplay, SystemPart, ToolChoice, ToolDef,
    };

    fn target(model: &str) -> Target {
        Target::new("oa", Protocol::Responses, Flavor::OpenAi, model)
    }

    fn ir() -> IrRequest {
        let mut r = IrRequest::new("claude-x", Protocol::Anthropic);
        r.max_output_tokens = 100;
        r.turns.push(Turn::User(vec![Part::Text("hi".into())]));
        r
    }

    fn built(r: &IrRequest) -> Json {
        serde_json::from_slice(&build(r, &target("gpt-4.1")).unwrap().body).unwrap()
    }

    #[test]
    fn builds_a_minimal_request_with_store_false_and_encrypted_reasoning() {
        let b = built(&ir());
        assert_eq!(b["model"], "gpt-4.1");
        assert_eq!(b["store"], false);
        assert_eq!(b["include"], json!(["reasoning.encrypted_content"]));
        assert_eq!(b["max_output_tokens"], 100);
        assert!(b.get("max_tokens").is_none() && b.get("max_completion_tokens").is_none());
        assert_eq!(
            b["input"],
            json!([{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}])
        );
        assert_eq!(build(&ir(), &target("m")).unwrap().path, "/v1/responses");
    }

    #[test]
    fn stop_sequences_are_rejected() {
        let mut r = ir();
        r.stop = vec!["x".into()];
        assert_eq!(build(&r, &target("m")).unwrap_err().field, "stop_sequences");
    }

    #[test]
    fn an_unset_limit_is_omitted_and_a_cap_clamps() {
        let mut r = ir();
        r.max_output_tokens = 0;
        let b: Json = serde_json::from_slice(&build(&r, &target("m")).unwrap().body).unwrap();
        assert!(b.get("max_output_tokens").is_none());
        let mut t = target("m");
        t.max_output_tokens_cap = Some(7);
        let b: Json = serde_json::from_slice(&build(&ir(), &t).unwrap().body).unwrap();
        assert_eq!(b["max_output_tokens"], 7);
    }

    #[test]
    fn leading_system_becomes_instructions_and_later_system_becomes_developer_items() {
        let mut r = ir();
        r.system = vec![
            SystemPart {
                text: "lead".into(),
                after_turns: 0,
                effort: None,
            },
            SystemPart {
                text: "mid".into(),
                after_turns: 2,
                effort: None,
            },
            SystemPart {
                text: "tail".into(),
                after_turns: 3,
                effort: None,
            },
        ];
        r.turns.push(Turn::Assistant(vec![Part::Text("a".into())]));
        r.turns.push(Turn::User(vec![Part::Text("b".into())]));
        let b = built(&r);
        assert_eq!(b["instructions"], "lead");
        let input = b["input"].as_array().unwrap();
        let roles: Vec<_> = input.iter().map(|i| i["role"].as_str().unwrap()).collect();
        assert_eq!(
            roles,
            ["user", "assistant", "developer", "user", "developer"]
        );
    }

    #[test]
    fn sampling_follows_the_model_family_and_top_k_is_ignored() {
        let mut r = ir();
        r.sampling.temperature = Some(0.3);
        r.sampling.top_p = Some(0.8);
        r.sampling.top_k = Some(5);
        let u = build(&r, &target("gpt-4.1")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert_eq!(b["temperature"], 0.3);
        assert_eq!(b["top_p"], 0.8);
        assert_eq!(u.ignored[0].name, "top_k");
        let u = build(&r, &target("o3")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert!(b.get("temperature").is_none());
        let names: Vec<_> = u.ignored.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["temperature", "top_p", "top_k"]);
    }

    #[test]
    fn images_and_documents_become_input_content() {
        let mut r = ir();
        r.turns[0] = Turn::User(vec![
            Part::Text("see".into()),
            Part::Image(ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AA".into(),
            }),
            Part::Document(DocumentSource::Text("doc".into())),
        ]);
        let c = &built(&r)["input"][0]["content"];
        assert_eq!(
            c[1],
            json!({"type": "input_image", "image_url": "data:image/png;base64,AA"})
        );
        assert_eq!(c[2], json!({"type": "input_text", "text": "doc"}));
    }

    #[test]
    fn tools_are_flat_function_definitions() {
        let mut r = ir();
        r.tools = vec![ToolDef {
            name: "read".into(),
            description: Some("d".into()),
            input_schema: json!({"type": "object"}),
            strict: Some(true),
        }];
        r.tool_choice = ToolChoice {
            kind: ToolChoiceKind::Named("read".into()),
            parallel: false,
        };
        let b = built(&r);
        assert_eq!(
            b["tools"][0],
            json!({"type": "function", "name": "read", "description": "d", "parameters": {"type": "object"}, "strict": true})
        );
        assert_eq!(
            b["tool_choice"],
            json!({"type": "function", "name": "read"})
        );
        assert_eq!(b["parallel_tool_calls"], false);
        r.tool_choice.kind = ToolChoiceKind::Required;
        assert_eq!(built(&r)["tool_choice"], "required");
        r.tool_choice.kind = ToolChoiceKind::None;
        assert_eq!(built(&r)["tool_choice"], "none");
        r.tools.clear();
        assert!(built(&r).get("tool_choice").is_none());
    }

    #[test]
    fn tool_calls_and_results_are_items_and_long_names_are_shortened() {
        let long = format!("mcp__{}", "z".repeat(80));
        let mut r = ir();
        r.tools = vec![ToolDef {
            name: long.clone(),
            description: None,
            input_schema: json!({}),
            strict: None,
        }];
        r.turns.push(Turn::Assistant(vec![
            Part::Text("calling".into()),
            Part::ToolCall {
                id: "c1".into(),
                name: long.clone(),
                input: json!({"a": 1}),
            },
        ]));
        r.turns.push(Turn::ToolResults(vec![ToolResult {
            call_id: "c1".into(),
            content: vec![
                Part::Text("done".into()),
                Part::Image(ImageSource::Url("https://i".into())),
            ],
            is_error: true,
        }]));
        let u = build(&r, &target("m")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        let input = b["input"].as_array().unwrap();
        assert_eq!(input[1]["content"][0]["text"], "calling");
        assert_eq!(input[2]["type"], "function_call");
        let short = input[2]["name"].as_str().unwrap();
        assert_eq!(short.chars().count(), 64);
        assert_eq!(u.tool_names.restore(short), long);
        assert_eq!(input[2]["arguments"], "{\"a\":1}");
        assert_eq!(
            input[3],
            json!({"type": "function_call_output", "call_id": "c1", "output": "[error] done"})
        );
        assert_eq!(
            input[4]["content"][1],
            json!({"type": "input_image", "image_url": "https://i"})
        );
    }

    fn effort_of(r: &IrRequest, t: &Target) -> Option<Json> {
        let b: Json = serde_json::from_slice(&build(r, t).unwrap().body).unwrap();
        b.get("reasoning").cloned()
    }

    #[test]
    fn reasoning_effort_and_summaries() {
        let mut r = ir();
        assert_eq!(effort_of(&r, &target("m")), None);
        r.reasoning = Reasoning {
            mode: ReasoningMode::On {
                budget_tokens: Some(16_000),
            },
            effort: None,
            display: ReasoningDisplay::Summarized,
        };
        assert_eq!(
            effort_of(&r, &target("m")),
            Some(json!({"effort": "high", "summary": "auto"}))
        );
        r.reasoning.effort = Some(Effort::Max);
        assert_eq!(
            effort_of(&r, &target("m")),
            Some(json!({"effort": "xhigh", "summary": "auto"}))
        );
        r.reasoning.mode = ReasoningMode::Adaptive;
        r.reasoning.effort = None;
        assert_eq!(
            effort_of(&r, &target("m")),
            Some(json!({"summary": "auto"}))
        );
        r.reasoning.mode = ReasoningMode::Off;
        assert_eq!(effort_of(&r, &target("m")), None);
        r.reasoning.mode = ReasoningMode::Unspecified;
        r.reasoning.effort = Some(Effort::Low);
        assert_eq!(effort_of(&r, &target("m")), Some(json!({"effort": "low"})));
    }

    #[test]
    fn structured_output_and_cache_key() {
        let mut r = ir();
        r.response_format = ResponseFormat::JsonSchema {
            schema: json!({"type": "object"}),
            strict: true,
        };
        r.cache_hints = true;
        let b = built(&r);
        assert_eq!(
            b["text"]["format"],
            json!({"type": "json_schema", "name": "response", "schema": {"type": "object"}, "strict": true})
        );
        assert_eq!(b["prompt_cache_key"], prompt_cache_key(&r));
    }

    fn replay(origin: Option<Origin>, opaque: Option<Vec<u8>>, text: &str) -> Vec<Json> {
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![
            Part::Reasoning {
                text: text.into(),
                display: ReasoningDisplay::Summarized,
                origin,
                opaque,
            },
            Part::Text("answer".into()),
        ]));
        let u = build(&r, &target("m")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        let mut items = b["input"].as_array().unwrap().clone();
        items
            .push(json!({"ignored": u.ignored.iter().map(|i| i.name.clone()).collect::<Vec<_>>()}));
        items
    }

    #[test]
    fn reasoning_from_this_upstream_is_rebuilt_from_its_encrypted_content() {
        let here = Some(Origin {
            upstream: "oa".into(),
            protocol: Protocol::Responses,
        });
        let items = replay(here.clone(), Some(b"gAAAA-enc".to_vec()), "summary");
        assert_eq!(
            items[1],
            json!({"type": "reasoning", "summary": [{"type": "summary_text", "text": "summary"}], "encrypted_content": "gAAAA-enc"})
        );
        assert_eq!(items[2]["role"], "assistant");
        assert_eq!(items[3], json!({"ignored": []}));
        let items = replay(here, Some(b"enc".to_vec()), "");
        assert_eq!(items[1]["summary"], json!([]));
    }

    #[test]
    fn reasoning_without_encrypted_content_or_from_elsewhere_is_dropped_and_reported() {
        let here = Some(Origin {
            upstream: "oa".into(),
            protocol: Protocol::Responses,
        });
        let items = replay(here, None, "x");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[2], json!({"ignored": ["thinking_not_replayed"]}));
        for origin in [
            None,
            Some(Origin {
                upstream: "oa".into(),
                protocol: Protocol::Chat,
            }),
            Some(Origin {
                upstream: "other".into(),
                protocol: Protocol::Responses,
            }),
        ] {
            let items = replay(origin, Some(b"enc".to_vec()), "x");
            assert_eq!(items[2], json!({"ignored": ["thinking_foreign_origin"]}));
        }
    }
}
