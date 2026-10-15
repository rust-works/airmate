//! The Anthropic outbound adapter: translating the IR into a Messages request.
//!
//! This is for clients that speak another protocol to an upstream that speaks Anthropic's (for
//! example an OpenAI SDK pointed at Fireworks' Anthropic-compatible surface). Anthropic to
//! Anthropic is a passthrough instead; see [`super::anthropic`].

use serde_json::{json, Map, Value as Json};

use super::chat::reject;
use super::{Target, UpstreamRequest};
use crate::ir::{
    DocumentSource, Effort, ImageSource, IrRequest, Part, ReasoningMode, ResponseFormat,
    ToolChoiceKind, ToolResult, Turn,
};
use crate::policy::{Policy, PolicyError};
use crate::toolname::ToolNames;

/// The Messages path.
pub const PATH: &str = "/v1/messages";

/// What airmate sends as `max_tokens` when the client set none: Anthropic requires one.
pub const DEFAULT_MAX_TOKENS: u32 = 8192;

/// The marker that distinguishes a `redacted_thinking` payload from a thinking signature in
/// the opaque state a reasoning part carries.
pub const REDACTED_PREFIX: &[u8] = b"redacted:";

/// The thinking budget used for an effort when the client gave none.
#[must_use]
pub fn budget_for(effort: Effort) -> u32 {
    match effort {
        Effort::Low => 1_024,
        Effort::Medium => 4_096,
        Effort::High => 16_000,
        Effort::XHigh => 32_000,
        Effort::Max => 64_000,
    }
}

fn source_json(src: &ImageSource) -> Json {
    match src {
        ImageSource::Base64 { media_type, data } => {
            json!({"type": "base64", "media_type": media_type, "data": data})
        }
        ImageSource::Url(u) => json!({"type": "url", "url": u}),
    }
}

fn content_blocks(parts: &[Part]) -> Vec<Json> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            Part::Text(t) | Part::Document(DocumentSource::Text(t)) => {
                out.push(json!({"type": "text", "text": t}));
            }
            Part::Image(i) => out.push(json!({"type": "image", "source": source_json(i)})),
            _ => {}
        }
    }
    out
}

fn tool_result_block(r: &ToolResult) -> Json {
    let mut b = json!({
        "type": "tool_result",
        "tool_use_id": r.call_id,
        "content": content_blocks(&r.content),
    });
    if r.is_error {
        b["is_error"] = json!(true);
    }
    b
}

/// Adds `blocks` to a user message, merging into the previous one when it is also from the user
/// (Anthropic requires strict alternation). Tool results must come first in a user message.
fn push_user(messages: &mut Vec<Json>, mut blocks: Vec<Json>, tool_results_first: bool) {
    if blocks.is_empty() {
        return;
    }
    if let Some(last) = messages.last_mut() {
        if last["role"] == "user" {
            let existing = last["content"].as_array_mut().expect("content is an array");
            if tool_results_first {
                blocks.append(existing);
                *existing = blocks;
            } else {
                existing.append(&mut blocks);
            }
            return;
        }
    }
    messages.push(json!({"role": "user", "content": blocks}));
}

fn assistant_blocks(
    parts: &[Part],
    target: &Target,
    names: &mut ToolNames,
    ignored: &mut Policy,
) -> Vec<Json> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            Part::Text(t) => out.push(json!({"type": "text", "text": t})),
            Part::ToolCall { id, name, input } => out.push(json!({
                "type": "tool_use",
                "id": id,
                "name": names.shorten(name),
                "input": input,
            })),
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
                    continue;
                }
                match opaque
                    .as_ref()
                    .and_then(|b| String::from_utf8(b.clone()).ok())
                {
                    Some(o) if o.as_bytes().starts_with(REDACTED_PREFIX) => out.push(json!({
                        "type": "redacted_thinking",
                        "data": &o[REDACTED_PREFIX.len()..],
                    })),
                    Some(sig) => {
                        out.push(json!({"type": "thinking", "thinking": text, "signature": sig}))
                    }
                    None => ignored.ignore(
                        "thinking_not_replayed",
                        "the reasoning carries no signature to replay",
                    ),
                }
            }
            Part::Image(_) | Part::Document(_) => {}
        }
    }
    out
}

/// Builds a Messages request for `target`.
///
/// # Errors
///
/// A [`PolicyError`] for something the target cannot take that must not be dropped silently.
pub fn build(req: &IrRequest, target: &Target) -> Result<UpstreamRequest, PolicyError> {
    let mut ignored = Policy::collector();
    let mut names = ToolNames::new();
    let mut body = Map::new();

    body.insert("model".into(), json!(target.model));
    body.insert("stream".into(), json!(req.stream));
    let mut max = if req.max_output_tokens == 0 {
        DEFAULT_MAX_TOKENS
    } else {
        req.max_output_tokens
    };
    if let Some(cap) = target.max_output_tokens_cap {
        max = max.min(cap);
    }
    if max == 0 {
        return Err(reject("max_tokens", "`max_tokens` must be greater than 0"));
    }
    body.insert("max_tokens".into(), json!(max));

    // System: leading parts become `system`; later ones are sent as user text.
    let lead: Vec<Json> = req
        .system
        .iter()
        .filter(|s| s.after_turns == 0)
        .map(|s| json!({"type": "text", "text": s.text}))
        .collect();
    let mut lead = lead;
    if req.cache_hints {
        if let Some(Json::Object(last)) = lead.last_mut() {
            last.insert("cache_control".into(), json!({"type": "ephemeral"}));
        }
    }
    if !lead.is_empty() {
        body.insert("system".into(), Json::Array(lead));
    }

    let mut messages: Vec<Json> = Vec::new();
    for (i, turn) in req.turns.iter().enumerate() {
        for s in req.system.iter().filter(|s| s.after_turns == i && i != 0) {
            ignored.ignore(
                "system_position",
                "a mid-conversation system message is sent as user text",
            );
            push_user(
                &mut messages,
                vec![json!({"type": "text", "text": format!("[system] {}", s.text)})],
                false,
            );
        }
        match turn {
            Turn::User(parts) => push_user(&mut messages, content_blocks(parts), false),
            Turn::Assistant(parts) => {
                let blocks = assistant_blocks(parts, target, &mut names, &mut ignored);
                if !blocks.is_empty() {
                    messages.push(json!({"role": "assistant", "content": blocks}));
                }
            }
            Turn::ToolResults(results) => {
                push_user(
                    &mut messages,
                    results.iter().map(tool_result_block).collect(),
                    true,
                );
            }
        }
    }
    for s in req
        .system
        .iter()
        .filter(|s| s.after_turns >= req.turns.len() && s.after_turns != 0)
    {
        ignored.ignore(
            "system_position",
            "a mid-conversation system message is sent as user text",
        );
        push_user(
            &mut messages,
            vec![json!({"type": "text", "text": format!("[system] {}", s.text)})],
            false,
        );
    }
    body.insert("messages".into(), Json::Array(messages));

    // Sampling.
    if let Some(t) = req.sampling.temperature {
        body.insert("temperature".into(), json!(t));
    }
    if let Some(p) = req.sampling.top_p {
        body.insert("top_p".into(), json!(p));
    }
    if let Some(k) = req.sampling.top_k {
        body.insert("top_k".into(), json!(k));
    }
    for (name, present) in [
        ("seed", req.sampling.seed.is_some()),
        (
            "frequency_penalty",
            req.sampling.frequency_penalty.is_some(),
        ),
        ("presence_penalty", req.sampling.presence_penalty.is_some()),
    ] {
        if present {
            ignored.ignore(name, "the Anthropic Messages API has no equivalent");
        }
    }
    if !req.stop.is_empty() {
        body.insert("stop_sequences".into(), json!(req.stop));
    }

    // Tools.
    if !req.tools.is_empty() {
        let mut tools: Vec<Json> = req
            .tools
            .iter()
            .map(|t| {
                let mut o = Map::new();
                o.insert("name".into(), json!(names.shorten(&t.name)));
                if let Some(d) = &t.description {
                    o.insert("description".into(), json!(d));
                }
                o.insert("input_schema".into(), t.input_schema.clone());
                if t.strict == Some(true) {
                    o.insert("strict".into(), json!(true));
                }
                Json::Object(o)
            })
            .collect();
        if req.cache_hints {
            if let Some(Json::Object(last)) = tools.last_mut() {
                last.insert("cache_control".into(), json!({"type": "ephemeral"}));
            }
        }
        body.insert("tools".into(), Json::Array(tools));
        let mut choice = match &req.tool_choice.kind {
            ToolChoiceKind::Auto => Some(json!({"type": "auto"})),
            ToolChoiceKind::Required => Some(json!({"type": "any"})),
            ToolChoiceKind::None => Some(json!({"type": "none"})),
            ToolChoiceKind::Named(n) => Some(json!({"type": "tool", "name": names.shorten(n)})),
        };
        if !req.tool_choice.parallel {
            if let Some(c) = choice.as_mut() {
                if c["type"] != "none" {
                    c["disable_parallel_tool_use"] = json!(true);
                }
            }
        }
        if req.tool_choice.kind != ToolChoiceKind::Auto || !req.tool_choice.parallel {
            if let Some(c) = choice {
                body.insert("tool_choice".into(), c);
            }
        }
    }

    // Reasoning.
    let effort = req
        .system
        .iter()
        .rev()
        .find_map(|s| s.effort)
        .or(req.reasoning.effort);
    match req.reasoning.mode {
        ReasoningMode::On { budget_tokens } => {
            let wanted = budget_tokens
                .or_else(|| effort.map(budget_for))
                .unwrap_or(budget_for(Effort::Medium));
            // The budget must be at least 1,024 and below max_tokens.
            let budget = wanted.max(1_024);
            if budget >= max {
                ignored.ignore(
                    "thinking",
                    "max_tokens leaves no room for the thinking budget",
                );
            } else {
                let mut t = json!({"type": "enabled", "budget_tokens": budget});
                if req.reasoning.display != crate::ir::ReasoningDisplay::Summarized {
                    t["display"] = json!(req.reasoning.display.as_str());
                }
                body.insert("thinking".into(), t);
            }
        }
        ReasoningMode::Adaptive => {
            body.insert("thinking".into(), json!({"type": "adaptive"}));
        }
        ReasoningMode::Off => {
            body.insert("thinking".into(), json!({"type": "disabled"}));
        }
        ReasoningMode::Unspecified => {}
    }
    if req.response_format == ResponseFormat::JsonObject {
        ignored.ignore(
            "response_format",
            "the Anthropic Messages API has no json_object mode",
        );
    }
    let mut output_config = Map::new();
    if let Some(e) = effort {
        output_config.insert("effort".into(), json!(e.as_str()));
    }
    if let ResponseFormat::JsonSchema { schema, .. } = &req.response_format {
        output_config.insert(
            "format".into(),
            json!({"type": "json_schema", "schema": schema}),
        );
    }
    if !output_config.is_empty() {
        body.insert("output_config".into(), Json::Object(output_config));
    }

    Ok(UpstreamRequest {
        path: PATH.to_owned(),
        body: serde_json::to_vec(&Json::Object(body)).expect("a JSON value serialises"),
        stream: req.stream,
        tool_names: names,
        ignored: ignored.into_ignored(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;
    use crate::ir::{
        Origin, Protocol, Reasoning, ReasoningDisplay, SystemPart, ToolChoice, ToolDef,
    };

    fn target() -> Target {
        Target::new("fw", Protocol::Anthropic, Flavor::Fireworks, "glm-5p1")
    }

    fn ir() -> IrRequest {
        let mut r = IrRequest::new("gpt-4.1", Protocol::Chat);
        r.max_output_tokens = 2000;
        r.turns.push(Turn::User(vec![Part::Text("hi".into())]));
        r
    }

    fn built(r: &IrRequest) -> Json {
        serde_json::from_slice(&build(r, &target()).unwrap().body).unwrap()
    }

    #[test]
    fn builds_a_minimal_request() {
        let b = built(&ir());
        assert_eq!(b["model"], "glm-5p1");
        assert_eq!(b["max_tokens"], 2000);
        assert_eq!(b["stream"], false);
        assert_eq!(
            b["messages"],
            json!([{"role": "user", "content": [{"type": "text", "text": "hi"}]}])
        );
        assert!(
            b.get("system").is_none() && b.get("tools").is_none() && b.get("thinking").is_none()
        );
        assert_eq!(build(&ir(), &target()).unwrap().path, "/v1/messages");
    }

    #[test]
    fn an_unset_limit_gets_the_default_because_anthropic_requires_one() {
        let mut r = ir();
        r.max_output_tokens = 0;
        assert_eq!(built(&r)["max_tokens"], DEFAULT_MAX_TOKENS);
        let mut t = target();
        t.max_output_tokens_cap = Some(100);
        assert_eq!(
            serde_json::from_slice::<Json>(&build(&r, &t).unwrap().body).unwrap()["max_tokens"],
            100
        );
        t.max_output_tokens_cap = Some(0);
        assert_eq!(build(&r, &t).unwrap_err().field, "max_tokens");
    }

    #[test]
    fn leading_system_is_the_system_field_and_later_system_becomes_user_text_and_is_reported() {
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
        ];
        r.turns.push(Turn::Assistant(vec![Part::Text("a".into())]));
        r.turns.push(Turn::User(vec![Part::Text("b".into())]));
        let u = build(&r, &target()).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert_eq!(b["system"], json!([{"type": "text", "text": "lead"}]));
        assert_eq!(b["messages"][2]["content"][0]["text"], "[system] mid");
        assert_eq!(
            b["messages"][2]["content"][1]["text"], "b",
            "merged into the next user message"
        );
        assert_eq!(u.ignored[0].name, "system_position");
    }

    #[test]
    fn consecutive_user_turns_merge_to_keep_the_roles_alternating() {
        let mut r = ir();
        r.turns.push(Turn::User(vec![Part::Text("second".into())]));
        let msgs = built(&r)["messages"].clone();
        assert_eq!(msgs.as_array().unwrap().len(), 1);
        assert_eq!(msgs[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn tool_results_come_first_in_their_user_message() {
        let mut r = ir();
        r.turns = vec![
            Turn::User(vec![Part::Text("go".into())]),
            Turn::Assistant(vec![Part::ToolCall {
                id: "t1".into(),
                name: "read".into(),
                input: json!({"p": 1}),
            }]),
            Turn::ToolResults(vec![ToolResult {
                call_id: "t1".into(),
                content: vec![Part::Text("data".into())],
                is_error: true,
            }]),
            Turn::User(vec![Part::Text("thanks".into())]),
        ];
        let m = built(&r)["messages"].clone();
        assert_eq!(
            m[1]["content"][0],
            json!({"type": "tool_use", "id": "t1", "name": "read", "input": {"p": 1}})
        );
        assert_eq!(m[2]["content"][0]["type"], "tool_result");
        assert_eq!(m[2]["content"][0]["tool_use_id"], "t1");
        assert_eq!(m[2]["content"][0]["is_error"], true);
        assert_eq!(
            m[2]["content"][1],
            json!({"type": "text", "text": "thanks"})
        );
        // A user turn that precedes tool results is pulled behind them.
        let mut r2 = r.clone();
        r2.turns.swap(2, 3);
        let m2 = built(&r2)["messages"].clone();
        assert_eq!(m2.as_array().unwrap().len(), 3);
    }

    #[test]
    fn images_keep_their_source() {
        let mut r = ir();
        r.turns[0] = Turn::User(vec![
            Part::Image(ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AA".into(),
            }),
            Part::Image(ImageSource::Url("https://x/y.png".into())),
        ]);
        let c = built(&r)["messages"][0]["content"].clone();
        assert_eq!(
            c[0]["source"],
            json!({"type": "base64", "media_type": "image/png", "data": "AA"})
        );
        assert_eq!(
            c[1]["source"],
            json!({"type": "url", "url": "https://x/y.png"})
        );
    }

    #[test]
    fn tools_and_choice_map_to_the_anthropic_forms() {
        let mut r = ir();
        r.tools = vec![ToolDef {
            name: "read".into(),
            description: Some("d".into()),
            input_schema: json!({"type": "object"}),
            strict: Some(true),
        }];
        let b = built(&r);
        assert_eq!(
            b["tools"][0],
            json!({"name": "read", "description": "d", "input_schema": {"type": "object"}, "strict": true})
        );
        assert!(b.get("tool_choice").is_none(), "auto is the default");
        for (kind, parallel, want) in [
            (ToolChoiceKind::Required, true, json!({"type": "any"})),
            (
                ToolChoiceKind::Named("read".into()),
                false,
                json!({"type": "tool", "name": "read", "disable_parallel_tool_use": true}),
            ),
            (ToolChoiceKind::None, false, json!({"type": "none"})),
            (
                ToolChoiceKind::Auto,
                false,
                json!({"type": "auto", "disable_parallel_tool_use": true}),
            ),
        ] {
            r.tool_choice = ToolChoice { kind, parallel };
            assert_eq!(built(&r)["tool_choice"], want);
        }
    }

    #[test]
    fn long_tool_names_are_shortened_and_mapped_back() {
        let long = format!("mcp__{}", "q".repeat(80));
        let mut r = ir();
        r.tools = vec![ToolDef {
            name: long.clone(),
            description: None,
            input_schema: json!({}),
            strict: None,
        }];
        let u = build(&r, &target()).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        let short = b["tools"][0]["name"].as_str().unwrap();
        assert_eq!(short.chars().count(), 64);
        assert_eq!(u.tool_names.restore(short), long);
    }

    #[test]
    fn thinking_maps_from_the_reasoning_request() {
        let with = |mode, effort| {
            let mut r = ir();
            r.max_output_tokens = 40_000;
            r.reasoning = Reasoning {
                mode,
                effort,
                display: ReasoningDisplay::Summarized,
            };
            built(&r)
        };
        assert_eq!(
            with(
                ReasoningMode::On {
                    budget_tokens: Some(5000)
                },
                None
            )["thinking"],
            json!({"type": "enabled", "budget_tokens": 5000})
        );
        let mut roomy = ir();
        roomy.max_output_tokens = 40_000;
        roomy.reasoning = Reasoning {
            mode: ReasoningMode::On {
                budget_tokens: None,
            },
            effort: Some(Effort::High),
            display: ReasoningDisplay::Summarized,
        };
        assert_eq!(
            built(&roomy)["thinking"]["budget_tokens"],
            16_000,
            "the budget is bucketed from the effort"
        );
        assert_eq!(
            with(ReasoningMode::Adaptive, Some(Effort::Low))["thinking"],
            json!({"type": "adaptive"})
        );
        assert_eq!(
            with(ReasoningMode::Adaptive, Some(Effort::Low))["output_config"]["effort"],
            "low"
        );
        assert_eq!(
            with(ReasoningMode::Off, None)["thinking"],
            json!({"type": "disabled"})
        );
        assert!(with(ReasoningMode::Unspecified, None)
            .get("thinking")
            .is_none());
    }

    #[test]
    fn a_budget_that_does_not_fit_under_max_tokens_is_dropped_and_reported() {
        let mut r = ir();
        r.max_output_tokens = 1000;
        r.reasoning = Reasoning {
            mode: ReasoningMode::On {
                budget_tokens: Some(5000),
            },
            effort: None,
            display: ReasoningDisplay::Summarized,
        };
        let u = build(&r, &target()).unwrap();
        assert!(serde_json::from_slice::<Json>(&u.body)
            .unwrap()
            .get("thinking")
            .is_none());
        assert_eq!(u.ignored[0].name, "thinking");
        r.max_output_tokens = 4000;
        r.reasoning.mode = ReasoningMode::On {
            budget_tokens: Some(3000),
        };
        r.reasoning.display = ReasoningDisplay::Omitted;
        let b = built(&r);
        assert_eq!(b["thinking"]["display"], "omitted");
    }

    #[test]
    fn sampling_stop_and_unsupported_parameters() {
        let mut r = ir();
        r.sampling.temperature = Some(0.5);
        r.sampling.top_p = Some(0.9);
        r.sampling.top_k = Some(20);
        r.sampling.seed = Some(7);
        r.sampling.frequency_penalty = Some(0.1);
        r.stop = vec!["END".into()];
        let u = build(&r, &target()).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert_eq!(
            (
                b["temperature"].as_f64(),
                b["top_p"].as_f64(),
                b["top_k"].as_u64()
            ),
            (Some(0.5), Some(0.9), Some(20))
        );
        assert_eq!(b["stop_sequences"], json!(["END"]));
        let names: Vec<_> = u.ignored.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["seed", "frequency_penalty"]);
    }

    #[test]
    fn structured_output_and_cache_hints() {
        let mut r = ir();
        r.response_format = ResponseFormat::JsonSchema {
            schema: json!({"type": "object"}),
            strict: true,
        };
        r.cache_hints = true;
        r.system = vec![SystemPart {
            text: "s".into(),
            after_turns: 0,
            effort: None,
        }];
        r.tools = vec![ToolDef {
            name: "t".into(),
            description: None,
            input_schema: json!({}),
            strict: None,
        }];
        let b = built(&r);
        assert_eq!(
            b["output_config"]["format"],
            json!({"type": "json_schema", "schema": {"type": "object"}})
        );
        assert_eq!(
            b["system"][0]["cache_control"],
            json!({"type": "ephemeral"})
        );
        assert_eq!(b["tools"][0]["cache_control"], json!({"type": "ephemeral"}));
    }

    #[test]
    fn replayed_reasoning_uses_the_upstreams_own_signature_and_redacted_blocks_round_trip() {
        let here = Some(Origin {
            upstream: "fw".into(),
            protocol: Protocol::Anthropic,
        });
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![
            Part::Reasoning {
                text: "thoughts".into(),
                display: ReasoningDisplay::Summarized,
                origin: here.clone(),
                opaque: Some(b"SIG".to_vec()),
            },
            Part::Reasoning {
                text: String::new(),
                display: ReasoningDisplay::Omitted,
                origin: here.clone(),
                opaque: Some(b"redacted:ENCRYPTED".to_vec()),
            },
            Part::Text("answer".into()),
        ]));
        r.turns.push(Turn::User(vec![Part::Text("more".into())]));
        let u = build(&r, &target()).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert_eq!(
            b["messages"][1]["content"][0],
            json!({"type": "thinking", "thinking": "thoughts", "signature": "SIG"})
        );
        assert_eq!(
            b["messages"][1]["content"][1],
            json!({"type": "redacted_thinking", "data": "ENCRYPTED"})
        );
        assert!(u.ignored.is_empty());
    }

    #[test]
    fn foreign_or_unsigned_reasoning_is_dropped_and_reported() {
        for (origin, opaque, want) in [
            (
                Some(Origin {
                    upstream: "other".into(),
                    protocol: Protocol::Anthropic,
                }),
                Some(b"SIG".to_vec()),
                "thinking_foreign_origin",
            ),
            (None, None, "thinking_foreign_origin"),
            (
                Some(Origin {
                    upstream: "fw".into(),
                    protocol: Protocol::Anthropic,
                }),
                None,
                "thinking_not_replayed",
            ),
        ] {
            let mut r = ir();
            r.turns.push(Turn::Assistant(vec![
                Part::Reasoning {
                    text: "x".into(),
                    display: ReasoningDisplay::Summarized,
                    origin,
                    opaque,
                },
                Part::Text("a".into()),
            ]));
            r.turns.push(Turn::User(vec![Part::Text("q".into())]));
            let u = build(&r, &target()).unwrap();
            assert_eq!(u.ignored[0].name, want);
            let b: Json = serde_json::from_slice(&u.body).unwrap();
            assert_eq!(b["messages"][1]["content"].as_array().unwrap().len(), 1);
        }
    }
}
