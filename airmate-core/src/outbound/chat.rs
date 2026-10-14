//! The OpenAI Chat Completions outbound adapter: request building.

use serde_json::{json, Map, Value as Json};

use super::{Target, UpstreamRequest};
use crate::cache::prompt_cache_key;
use crate::flavor::{is_reasoning_family, ReasoningControl, StreamUsage, TokenLimitField};
use crate::ir::{
    DocumentSource, Effort, ImageSource, IrRequest, Part, Protocol, ReasoningMode, ResponseFormat,
    ToolChoiceKind, ToolResult, Turn,
};
use crate::policy::{Policy, PolicyError};
use crate::toolname::ToolNames;

/// The Chat Completions path.
pub const PATH: &str = "/v1/chat/completions";

pub(super) fn reject(field: &str, message: impl Into<String>) -> PolicyError {
    PolicyError {
        field: field.to_owned(),
        message: message.into(),
    }
}

/// Builds a Chat Completions request for `target`.
///
/// # Errors
///
/// A [`PolicyError`] when the request has something the target cannot take and that must not be
/// dropped silently (for example more stop sequences than the upstream accepts).
pub fn build(req: &IrRequest, target: &Target) -> Result<UpstreamRequest, PolicyError> {
    let mut ignored = Policy::collector();
    let mut names = ToolNames::new();
    let flavor = target.flavor;
    let reasoning_model = is_reasoning_family(&target.model);
    let mut body = Map::new();

    body.insert("model".into(), json!(target.model));
    body.insert(
        "messages".into(),
        Json::Array(messages(req, target, &mut names, &mut ignored)),
    );
    body.insert("stream".into(), json!(req.stream));
    if req.stream && flavor.stream_usage() == StreamUsage::IncludeUsageOption {
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }

    // 0 means the client did not set a limit (OpenAI-shaped inbounds): send none.
    let limit = target
        .max_output_tokens_cap
        .map_or(req.max_output_tokens, |c| {
            if req.max_output_tokens == 0 {
                c
            } else {
                c.min(req.max_output_tokens)
            }
        });
    if limit > 0 {
        let field = flavor.token_limit_field(Protocol::Chat, reasoning_model);
        debug_assert_ne!(field, TokenLimitField::MaxOutputTokens);
        body.insert(field.key().into(), json!(limit));
    }

    // Sampling.
    if reasoning_model {
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
    if let Some(k) = req.sampling.top_k {
        if flavor.sends_top_k() {
            body.insert("top_k".into(), json!(k));
        } else {
            ignored.ignore("top_k", "the upstream does not take top_k");
        }
    }
    if let Some(s) = req.sampling.seed {
        body.insert("seed".into(), json!(s));
    }
    if let Some(p) = req.sampling.frequency_penalty {
        body.insert("frequency_penalty".into(), json!(p));
    }
    if let Some(p) = req.sampling.presence_penalty {
        body.insert("presence_penalty".into(), json!(p));
    }

    if !req.stop.is_empty() {
        if let Some(cap) = flavor.stop_cap() {
            if req.stop.len() > cap {
                return Err(reject(
                    "stop_sequences",
                    format!(
                        "the upstream accepts at most {cap} stop sequences, got {}",
                        req.stop.len()
                    ),
                ));
            }
        }
        body.insert("stop".into(), json!(req.stop));
    }

    // Tools.
    if !req.tools.is_empty() {
        let tools: Vec<Json> = req
            .tools
            .iter()
            .map(|t| {
                let mut f = Map::new();
                f.insert("name".into(), json!(names.shorten(&t.name)));
                if let Some(d) = &t.description {
                    f.insert("description".into(), json!(d));
                }
                f.insert("parameters".into(), t.input_schema.clone());
                if let (Some(true), true) = (t.strict, flavor.strict_structured_output()) {
                    f.insert("strict".into(), json!(true));
                }
                json!({"type": "function", "function": Json::Object(f)})
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
                    json!({"type": "function", "function": {"name": names.shorten(n)}}),
                );
            }
        }
        if !req.tool_choice.parallel {
            body.insert("parallel_tool_calls".into(), json!(false));
        }
    }

    // Reasoning (5.2).
    reasoning(req, target, &mut body, &mut ignored);

    // Structured output.
    if let ResponseFormat::JsonSchema { schema, strict } = &req.response_format {
        body.insert(
            "response_format".into(),
            json!({"type": "json_schema", "json_schema": {
                "name": "response",
                "schema": schema,
                "strict": *strict && flavor.strict_structured_output(),
            }}),
        );
    }

    if req.cache_hints && flavor.sends_prompt_cache_key() {
        body.insert("prompt_cache_key".into(), json!(prompt_cache_key(req)));
    }
    if let Some(b) = flavor.context_overflow_behavior() {
        body.insert("context_length_exceeded_behavior".into(), json!(b));
    }

    Ok(UpstreamRequest {
        path: PATH.to_owned(),
        body: serde_json::to_vec(&Json::Object(body)).expect("a JSON value serialises"),
        stream: req.stream,
        tool_names: names,
        ignored: ignored.into_ignored(),
    })
}

/// The effort in force: a per-turn effort on a mid-conversation system message wins.
pub(super) fn effective_effort(req: &IrRequest) -> Option<Effort> {
    req.system
        .iter()
        .rev()
        .find_map(|s| s.effort)
        .or(req.reasoning.effort)
}

fn reasoning(req: &IrRequest, target: &Target, body: &mut Map<String, Json>, ignored: &mut Policy) {
    let accepted = target.accepted_efforts();
    let clamp = |e: Effort| e.clamp_down(accepted).map(|e| json!(e.as_str()));
    let effort = effective_effort(req);
    let fireworks = target.flavor.reasoning_control() == ReasoningControl::ThinkingOrEffort;

    match req.reasoning.mode {
        ReasoningMode::On { budget_tokens } => {
            if fireworks {
                let mut t = json!({"type": "enabled"});
                if let Some(b) = budget_tokens {
                    t["budget_tokens"] = json!(b);
                }
                body.insert("thinking".into(), t);
                if effort.is_some() {
                    ignored.ignore(
                        "effort",
                        "Fireworks takes `thinking` or `reasoning_effort`, never both",
                    );
                }
            } else if let Some(v) = effort
                .or_else(|| budget_tokens.map(Effort::from_budget))
                .and_then(clamp)
            {
                body.insert("reasoning_effort".into(), v);
            }
        }
        ReasoningMode::Adaptive => {
            if let Some(v) = effort.and_then(clamp) {
                body.insert("reasoning_effort".into(), v);
            }
        }
        ReasoningMode::Off => {
            if fireworks {
                body.insert("thinking".into(), json!({"type": "disabled"}));
            }
        }
        ReasoningMode::Unspecified => {
            if let Some(v) = effort.and_then(clamp) {
                body.insert("reasoning_effort".into(), v);
            }
        }
    }
}

// ---- messages -----------------------------------------------------------------------------

fn data_uri(media_type: &str, data: &str) -> String {
    format!("data:{media_type};base64,{data}")
}

pub(super) fn image_url(src: &ImageSource) -> String {
    match src {
        ImageSource::Base64 { media_type, data } => data_uri(media_type, data),
        ImageSource::Url(u) => u.clone(),
    }
}

/// Converts content parts to a string (text only) or an array of content parts (any image).
fn user_content(parts: &[Part]) -> Json {
    let has_image = parts.iter().any(|p| matches!(p, Part::Image(_)));
    if !has_image {
        return Json::String(text_of(parts));
    }
    let mut out = Vec::new();
    for p in parts {
        match p {
            Part::Text(t) => out.push(json!({"type": "text", "text": t})),
            Part::Document(DocumentSource::Text(t)) => out.push(json!({"type": "text", "text": t})),
            Part::Image(i) => {
                out.push(json!({"type": "image_url", "image_url": {"url": image_url(i)}}))
            }
            _ => {}
        }
    }
    Json::Array(out)
}

pub(super) fn text_of(parts: &[Part]) -> String {
    parts
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) | Part::Document(DocumentSource::Text(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn messages(
    req: &IrRequest,
    target: &Target,
    names: &mut ToolNames,
    ignored: &mut Policy,
) -> Vec<Json> {
    let mut out: Vec<Json> = Vec::new();
    let lead: Vec<&str> = req
        .system
        .iter()
        .filter(|s| s.after_turns == 0)
        .map(|s| s.text.as_str())
        .collect();
    if !lead.is_empty() {
        out.push(json!({"role": "system", "content": lead.join("\n\n")}));
    }
    for (i, turn) in req.turns.iter().enumerate() {
        for s in req.system.iter().filter(|s| s.after_turns == i && i != 0) {
            out.push(json!({"role": "system", "content": s.text}));
        }
        match turn {
            Turn::User(parts) => out.push(json!({"role": "user", "content": user_content(parts)})),
            Turn::Assistant(parts) => out.push(assistant(parts, target, names, ignored)),
            Turn::ToolResults(results) => tool_results(results, &mut out),
        }
    }
    for s in req
        .system
        .iter()
        .filter(|s| s.after_turns >= req.turns.len() && s.after_turns != 0)
    {
        out.push(json!({"role": "system", "content": s.text}));
    }
    out
}

fn assistant(parts: &[Part], target: &Target, names: &mut ToolNames, ignored: &mut Policy) -> Json {
    let mut text = Vec::new();
    let mut calls = Vec::new();
    let mut reasoning = Vec::new();
    for p in parts {
        match p {
            Part::Text(t) => text.push(t.as_str()),
            Part::ToolCall { id, name, input } => calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": names.shorten(name), "arguments": input.to_string()},
            })),
            Part::Reasoning {
                text: r, origin, ..
            } => {
                if origin.as_ref() != Some(&target.origin()) {
                    ignored.ignore(
                        "thinking_foreign_origin",
                        "the reasoning was produced by a different upstream",
                    );
                } else if !target.flavor.replays_reasoning_content() {
                    ignored.ignore(
                        "thinking_not_replayed",
                        "the upstream does not take reasoning back",
                    );
                } else {
                    reasoning.push(r.as_str());
                }
            }
            Part::Image(_) | Part::Document(_) => {}
        }
    }
    let mut m = Map::new();
    m.insert("role".into(), json!("assistant"));
    m.insert(
        "content".into(),
        if text.is_empty() && !calls.is_empty() {
            Json::Null
        } else {
            json!(text.join("\n\n"))
        },
    );
    if !calls.is_empty() {
        m.insert("tool_calls".into(), Json::Array(calls));
    }
    if !reasoning.is_empty() {
        m.insert("reasoning_content".into(), json!(reasoning.join("\n\n")));
    }
    Json::Object(m)
}

fn tool_results(results: &[ToolResult], out: &mut Vec<Json>) {
    let mut images: Vec<Json> = Vec::new();
    for r in results {
        let mut text = text_of(&r.content);
        if r.is_error {
            text = format!("[error] {text}");
        }
        out.push(json!({"role": "tool", "tool_call_id": r.call_id, "content": text}));
        for p in &r.content {
            if let Part::Image(i) = p {
                images.push(json!({"type": "image_url", "image_url": {"url": image_url(i)}}));
            }
        }
    }
    if !images.is_empty() {
        let mut content =
            vec![json!({"type": "text", "text": "Images returned by the tool calls above:"})];
        content.extend(images);
        out.push(json!({"role": "user", "content": content}));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;
    use crate::ir::{Origin, Reasoning, ReasoningDisplay, SystemPart, ToolChoice, ToolDef};

    fn target(flavor: Flavor, model: &str) -> Target {
        Target::new("up", Protocol::Chat, flavor, model)
    }

    fn ir() -> IrRequest {
        let mut r = IrRequest::new("claude-x", Protocol::Anthropic);
        r.max_output_tokens = 100;
        r.turns.push(Turn::User(vec![Part::Text("hi".into())]));
        r
    }

    fn built(r: &IrRequest, t: &Target) -> Json {
        serde_json::from_slice(&build(r, t).unwrap().body).unwrap()
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: Some("d".into()),
            input_schema: json!({"type": "object"}),
            strict: Some(true),
        }
    }

    #[test]
    fn builds_a_minimal_request() {
        let b = built(&ir(), &target(Flavor::Generic, "qwen3"));
        assert_eq!(b["model"], "qwen3");
        assert_eq!(b["messages"], json!([{"role": "user", "content": "hi"}]));
        assert_eq!(b["stream"], false);
        assert_eq!(b["max_tokens"], 100);
        assert!(b.get("max_completion_tokens").is_none() && b.get("stream_options").is_none());
        let u = build(&ir(), &target(Flavor::Generic, "m")).unwrap();
        assert_eq!(u.path, "/v1/chat/completions");
        assert!(!u.stream && u.ignored.is_empty());
    }

    #[test]
    fn sends_exactly_one_token_limit_field() {
        for (flavor, model, key) in [
            (Flavor::OpenAi, "gpt-4o", "max_completion_tokens"),
            (Flavor::Fireworks, "glm", "max_tokens"),
            (Flavor::Generic, "o3-mini", "max_completion_tokens"),
            (Flavor::Generic, "llama", "max_tokens"),
        ] {
            let b = built(&ir(), &target(flavor, model));
            let limits: Vec<&str> = ["max_tokens", "max_completion_tokens", "max_output_tokens"]
                .into_iter()
                .filter(|k| b.get(*k).is_some())
                .collect();
            assert_eq!(limits, vec![key], "{flavor:?} {model}");
        }
    }

    #[test]
    fn an_unset_limit_is_omitted_and_a_cap_clamps() {
        let mut r = ir();
        r.max_output_tokens = 0;
        let b = built(&r, &target(Flavor::Generic, "m"));
        assert!(b.get("max_tokens").is_none() && b.get("max_completion_tokens").is_none());
        let mut t = target(Flavor::Generic, "m");
        t.max_output_tokens_cap = Some(50);
        assert_eq!(built(&ir(), &t)["max_tokens"], 50);
        assert_eq!(
            built(&r, &t)["max_tokens"],
            50,
            "a cap applies even when the client set none"
        );
    }

    #[test]
    fn streaming_requests_usage_per_flavor() {
        let mut r = ir();
        r.stream = true;
        assert_eq!(
            built(&r, &target(Flavor::OpenAi, "m"))["stream_options"],
            json!({"include_usage": true})
        );
        assert_eq!(
            built(&r, &target(Flavor::Generic, "m"))["stream_options"],
            json!({"include_usage": true})
        );
        assert!(built(&r, &target(Flavor::Fireworks, "m"))
            .get("stream_options")
            .is_none());
        assert!(build(&r, &target(Flavor::Generic, "m")).unwrap().stream);
    }

    #[test]
    fn reasoning_models_drop_sampling_and_report_it() {
        let mut r = ir();
        r.sampling.temperature = Some(0.2);
        r.sampling.top_p = Some(0.9);
        let u = build(&r, &target(Flavor::OpenAi, "gpt-5")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert!(b.get("temperature").is_none() && b.get("top_p").is_none());
        let names: Vec<_> = u.ignored.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["temperature", "top_p"]);
        let b = built(&r, &target(Flavor::OpenAi, "gpt-4o"));
        assert_eq!(b["temperature"], 0.2);
        assert_eq!(b["top_p"], 0.9);
    }

    #[test]
    fn top_k_goes_to_fireworks_and_generic_only() {
        let mut r = ir();
        r.sampling.top_k = Some(40);
        assert_eq!(built(&r, &target(Flavor::Fireworks, "m"))["top_k"], 40);
        assert_eq!(built(&r, &target(Flavor::Generic, "m"))["top_k"], 40);
        let u = build(&r, &target(Flavor::OpenAi, "m")).unwrap();
        assert!(serde_json::from_slice::<Json>(&u.body)
            .unwrap()
            .get("top_k")
            .is_none());
        assert_eq!(u.ignored[0].name, "top_k");
    }

    #[test]
    fn stop_sequences_are_capped_where_the_upstream_caps_them() {
        let mut r = ir();
        r.stop = (0..5).map(|i| i.to_string()).collect();
        assert_eq!(
            build(&r, &target(Flavor::OpenAi, "m")).unwrap_err().field,
            "stop_sequences"
        );
        assert_eq!(
            build(&r, &target(Flavor::Fireworks, "m"))
                .unwrap_err()
                .field,
            "stop_sequences"
        );
        assert_eq!(
            built(&r, &target(Flavor::Generic, "m"))["stop"]
                .as_array()
                .unwrap()
                .len(),
            5
        );
        r.stop.truncate(4);
        assert_eq!(
            built(&r, &target(Flavor::OpenAi, "m"))["stop"]
                .as_array()
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn system_messages_are_merged_up_front_and_kept_in_place_mid_conversation() {
        let mut r = ir();
        r.system = vec![
            SystemPart {
                text: "one".into(),
                after_turns: 0,
                effort: None,
            },
            SystemPart {
                text: "two".into(),
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
        let b = built(&r, &target(Flavor::Generic, "m"));
        let roles: Vec<_> = b["messages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["role"].as_str().unwrap())
            .collect();
        assert_eq!(roles, ["system", "user", "assistant", "system", "user"]);
        assert_eq!(b["messages"][0]["content"], "one\n\ntwo");
        assert_eq!(b["messages"][3]["content"], "mid");
    }

    #[test]
    fn a_system_message_after_the_last_turn_is_kept() {
        let mut r = ir();
        r.system = vec![SystemPart {
            text: "tail".into(),
            after_turns: 1,
            effort: None,
        }];
        let b = built(&r, &target(Flavor::Generic, "m"));
        assert_eq!(
            b["messages"][1],
            json!({"role": "system", "content": "tail"})
        );
    }

    #[test]
    fn images_make_content_an_array() {
        let mut r = ir();
        r.turns[0] = Turn::User(vec![
            Part::Text("look".into()),
            Part::Image(ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }),
            Part::Image(ImageSource::Url("https://x/y.png".into())),
            Part::Document(DocumentSource::Text("doc".into())),
        ]);
        let c = &built(&r, &target(Flavor::Generic, "m"))["messages"][0]["content"];
        assert_eq!(c[0], json!({"type": "text", "text": "look"}));
        assert_eq!(c[1]["image_url"]["url"], "data:image/png;base64,AAAA");
        assert_eq!(c[2]["image_url"]["url"], "https://x/y.png");
        assert_eq!(c[3], json!({"type": "text", "text": "doc"}));
    }

    #[test]
    fn several_text_parts_are_joined() {
        let mut r = ir();
        r.turns[0] = Turn::User(vec![Part::Text("a".into()), Part::Text("b".into())]);
        assert_eq!(
            built(&r, &target(Flavor::Generic, "m"))["messages"][0]["content"],
            "a\n\nb"
        );
    }

    #[test]
    fn tools_choice_and_parallelism() {
        let mut r = ir();
        r.tools = vec![tool("read")];
        let b = built(&r, &target(Flavor::OpenAi, "m"));
        assert_eq!(b["tools"][0]["function"]["name"], "read");
        assert_eq!(b["tools"][0]["function"]["strict"], true);
        assert!(b.get("tool_choice").is_none() && b.get("parallel_tool_calls").is_none());
        assert!(
            built(&r, &target(Flavor::Generic, "m"))["tools"][0]["function"]
                .get("strict")
                .is_none()
        );

        r.tool_choice = ToolChoice {
            kind: ToolChoiceKind::Required,
            parallel: false,
        };
        let b = built(&r, &target(Flavor::Generic, "m"));
        assert_eq!(b["tool_choice"], "required");
        assert_eq!(b["parallel_tool_calls"], false);

        r.tool_choice.kind = ToolChoiceKind::Named("read".into());
        assert_eq!(
            built(&r, &target(Flavor::Generic, "m"))["tool_choice"],
            json!({"type": "function", "function": {"name": "read"}})
        );
        r.tool_choice.kind = ToolChoiceKind::None;
        assert_eq!(
            built(&r, &target(Flavor::Generic, "m"))["tool_choice"],
            "none"
        );
    }

    #[test]
    fn tool_choice_is_omitted_without_tools() {
        let mut r = ir();
        r.tool_choice = ToolChoice {
            kind: ToolChoiceKind::Required,
            parallel: false,
        };
        let b = built(&r, &target(Flavor::Generic, "m"));
        assert!(
            b.get("tool_choice").is_none()
                && b.get("parallel_tool_calls").is_none()
                && b.get("tools").is_none()
        );
    }

    #[test]
    fn long_tool_names_are_shortened_everywhere_and_mapped_back() {
        let long = format!("mcp__server__{}", "t".repeat(70));
        let mut r = ir();
        r.tools = vec![tool(&long)];
        r.tool_choice.kind = ToolChoiceKind::Named(long.clone());
        r.turns.push(Turn::Assistant(vec![Part::ToolCall {
            id: "c1".into(),
            name: long.clone(),
            input: json!({"a": 1}),
        }]));
        let u = build(&r, &target(Flavor::Generic, "m")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        let short = b["tools"][0]["function"]["name"].as_str().unwrap();
        assert_eq!(short.chars().count(), 64);
        assert_eq!(b["tool_choice"]["function"]["name"], short);
        assert_eq!(b["messages"][1]["tool_calls"][0]["function"]["name"], short);
        assert_eq!(u.tool_names.restore(short), long);
        assert_eq!(
            b["messages"][1]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
        assert_eq!(b["messages"][1]["content"], Json::Null);
    }

    #[test]
    fn assistant_text_and_tool_calls_share_a_message() {
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![
            Part::Text("calling".into()),
            Part::ToolCall {
                id: "c".into(),
                name: "n".into(),
                input: json!({}),
            },
        ]));
        let m = &built(&r, &target(Flavor::Generic, "m"))["messages"][1];
        assert_eq!(m["content"], "calling");
        assert_eq!(m["tool_calls"][0]["id"], "c");
        assert_eq!(m["tool_calls"][0]["type"], "function");
    }

    #[test]
    fn tool_results_become_tool_messages_with_errors_marked() {
        let mut r = ir();
        r.turns.push(Turn::ToolResults(vec![
            ToolResult {
                call_id: "a".into(),
                content: vec![Part::Text("ok".into())],
                is_error: false,
            },
            ToolResult {
                call_id: "b".into(),
                content: vec![
                    Part::Text("boom".into()),
                    Part::Image(ImageSource::Url("https://x/i.png".into())),
                ],
                is_error: true,
            },
        ]));
        let b = built(&r, &target(Flavor::Generic, "m"));
        let m = b["messages"].as_array().unwrap();
        assert_eq!(
            m[1],
            json!({"role": "tool", "tool_call_id": "a", "content": "ok"})
        );
        assert_eq!(
            m[2],
            json!({"role": "tool", "tool_call_id": "b", "content": "[error] boom"})
        );
        assert_eq!(m[3]["role"], "user");
        assert_eq!(m[3]["content"][1]["image_url"]["url"], "https://x/i.png");
    }

    fn with_reasoning(mode: ReasoningMode, effort: Option<Effort>) -> IrRequest {
        let mut r = ir();
        r.reasoning = Reasoning {
            mode,
            effort,
            display: ReasoningDisplay::Summarized,
        };
        r
    }

    #[test]
    fn enabled_thinking_maps_per_flavor() {
        let r = with_reasoning(
            ReasoningMode::On {
                budget_tokens: Some(16_000),
            },
            None,
        );
        let fw = built(&r, &target(Flavor::Fireworks, "m"));
        assert_eq!(
            fw["thinking"],
            json!({"type": "enabled", "budget_tokens": 16000})
        );
        assert!(fw.get("reasoning_effort").is_none());
        assert_eq!(
            built(&r, &target(Flavor::OpenAi, "m"))["reasoning_effort"],
            "high"
        );
        // generic tops out at high even when the budget buckets higher
        let big = with_reasoning(
            ReasoningMode::On {
                budget_tokens: Some(100_000),
            },
            None,
        );
        assert_eq!(
            built(&big, &target(Flavor::Generic, "m"))["reasoning_effort"],
            "high"
        );
        assert_eq!(
            built(&big, &target(Flavor::OpenAi, "m"))["reasoning_effort"],
            "xhigh"
        );
    }

    #[test]
    fn fireworks_never_gets_thinking_and_effort_together() {
        let r = with_reasoning(
            ReasoningMode::On {
                budget_tokens: Some(2000),
            },
            Some(Effort::High),
        );
        let u = build(&r, &target(Flavor::Fireworks, "m")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert!(b.get("thinking").is_some() && b.get("reasoning_effort").is_none());
        assert_eq!(u.ignored[0].name, "effort");
    }

    #[test]
    fn an_explicit_effort_beats_the_budget_and_is_clamped_down() {
        let r = with_reasoning(
            ReasoningMode::On {
                budget_tokens: Some(1_000),
            },
            Some(Effort::Max),
        );
        assert_eq!(
            built(&r, &target(Flavor::OpenAi, "m"))["reasoning_effort"],
            "xhigh"
        );
        assert_eq!(
            built(&r, &target(Flavor::Generic, "m"))["reasoning_effort"],
            "high"
        );
        let mut t = target(Flavor::Generic, "m");
        t.effort_levels = Some(vec![Effort::Medium]);
        assert_eq!(built(&r, &t)["reasoning_effort"], "medium");
    }

    #[test]
    fn adaptive_uses_effort_only_when_set() {
        let none = with_reasoning(ReasoningMode::Adaptive, None);
        for f in [Flavor::Generic, Flavor::OpenAi, Flavor::Fireworks] {
            let b = built(&none, &target(f, "m"));
            assert!(
                b.get("reasoning_effort").is_none() && b.get("thinking").is_none(),
                "{f:?}"
            );
        }
        let set = with_reasoning(ReasoningMode::Adaptive, Some(Effort::Medium));
        assert_eq!(
            built(&set, &target(Flavor::Fireworks, "m"))["reasoning_effort"],
            "medium"
        );
    }

    #[test]
    fn disabled_thinking_matters_only_to_fireworks() {
        let r = with_reasoning(ReasoningMode::Off, None);
        assert_eq!(
            built(&r, &target(Flavor::Fireworks, "m"))["thinking"],
            json!({"type": "disabled"})
        );
        for f in [Flavor::Generic, Flavor::OpenAi] {
            let b = built(&r, &target(f, "m"));
            assert!(b.get("thinking").is_none() && b.get("reasoning_effort").is_none());
        }
    }

    #[test]
    fn effort_without_thinking_still_sets_reasoning_effort() {
        let r = with_reasoning(ReasoningMode::Unspecified, Some(Effort::Low));
        assert_eq!(
            built(&r, &target(Flavor::Generic, "m"))["reasoning_effort"],
            "low"
        );
        assert!(built(&ir(), &target(Flavor::Generic, "m"))
            .get("reasoning_effort")
            .is_none());
    }

    #[test]
    fn a_per_turn_effort_on_a_system_message_wins() {
        let mut r = with_reasoning(ReasoningMode::Adaptive, Some(Effort::Low));
        r.system.push(SystemPart {
            text: "think".into(),
            after_turns: 1,
            effort: Some(Effort::High),
        });
        assert_eq!(
            built(&r, &target(Flavor::Generic, "m"))["reasoning_effort"],
            "high"
        );
    }

    #[test]
    fn structured_output_is_strict_only_where_supported() {
        let mut r = ir();
        r.response_format = ResponseFormat::JsonSchema {
            schema: json!({"type": "object"}),
            strict: true,
        };
        let o = built(&r, &target(Flavor::OpenAi, "m"));
        assert_eq!(o["response_format"]["json_schema"]["strict"], true);
        assert_eq!(
            o["response_format"]["json_schema"]["schema"],
            json!({"type": "object"})
        );
        assert_eq!(o["response_format"]["type"], "json_schema");
        assert_eq!(
            built(&r, &target(Flavor::Generic, "m"))["response_format"]["json_schema"]["strict"],
            false
        );
    }

    #[test]
    fn cache_hints_set_the_prompt_cache_key() {
        let mut r = ir();
        assert!(built(&r, &target(Flavor::OpenAi, "m"))
            .get("prompt_cache_key")
            .is_none());
        r.cache_hints = true;
        let b = built(&r, &target(Flavor::OpenAi, "m"));
        assert_eq!(b["prompt_cache_key"], prompt_cache_key(&r));
    }

    #[test]
    fn fireworks_asks_to_error_on_context_overflow() {
        assert_eq!(
            built(&ir(), &target(Flavor::Fireworks, "m"))["context_length_exceeded_behavior"],
            "error"
        );
        assert!(built(&ir(), &target(Flavor::OpenAi, "m"))
            .get("context_length_exceeded_behavior")
            .is_none());
    }

    fn history_with_reasoning(origin: Option<Origin>) -> IrRequest {
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![
            Part::Reasoning {
                text: "thoughts".into(),
                display: ReasoningDisplay::Summarized,
                origin,
                opaque: None,
            },
            Part::Text("answer".into()),
        ]));
        r.turns.push(Turn::User(vec![Part::Text("more".into())]));
        r
    }

    #[test]
    fn reasoning_from_the_same_upstream_is_replayed_where_supported() {
        let here = Some(Origin {
            upstream: "up".into(),
            protocol: Protocol::Chat,
        });
        let r = history_with_reasoning(here);
        let b = built(&r, &target(Flavor::Fireworks, "m"));
        assert_eq!(b["messages"][1]["reasoning_content"], "thoughts");
        let u = build(&r, &target(Flavor::OpenAi, "m")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert!(b["messages"][1].get("reasoning_content").is_none());
        assert_eq!(u.ignored[0].name, "thinking_not_replayed");
    }

    #[test]
    fn reasoning_from_another_upstream_is_dropped_and_reported() {
        for origin in [
            Some(Origin {
                upstream: "other".into(),
                protocol: Protocol::Chat,
            }),
            Some(Origin {
                upstream: "up".into(),
                protocol: Protocol::Responses,
            }),
            None,
        ] {
            let u = build(
                &history_with_reasoning(origin),
                &target(Flavor::Fireworks, "m"),
            )
            .unwrap();
            let b: Json = serde_json::from_slice(&u.body).unwrap();
            assert!(b["messages"][1].get("reasoning_content").is_none());
            assert_eq!(u.ignored[0].name, "thinking_foreign_origin");
        }
    }
}
