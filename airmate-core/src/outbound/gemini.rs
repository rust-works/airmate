//! The Google Gemini `generateContent` outbound adapter: request building.
//!
//! Facts here are as documented for the Gemini API on 2026-10-14 and have not been verified
//! against a live endpoint; the live conformance test in `tests/live.rs` does that when it is run
//! with a key.

use std::collections::HashMap;

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Map, Value as Json};

use super::chat::{effective_effort, reject};
use super::{Target, UpstreamRequest};
use crate::ir::{
    DocumentSource, Effort, ImageSource, IrRequest, Part, ReasoningDisplay, ReasoningMode,
    ResponseFormat, ToolChoiceKind, ToolResult, Turn,
};
use crate::policy::{Policy, PolicyError};
use crate::toolname::ToolNames;

/// The most stop sequences `generateContent` accepts.
pub const MAX_STOP_SEQUENCES: usize = 5;

/// The documented placeholder that lets a Gemini 3 function call without a recorded thought
/// signature through validation.
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// The request path for `model`: `generateContent`, or `streamGenerateContent` as server-sent
/// events.
#[must_use]
pub fn path(model: &str, stream: bool) -> String {
    let model = model.strip_prefix("models/").unwrap_or(model);
    if stream {
        format!("/v1beta/models/{model}:streamGenerateContent?alt=sse")
    } else {
        format!("/v1beta/models/{model}:generateContent")
    }
}

/// Builds a `generateContent` request for `target`.
///
/// # Errors
///
/// A [`PolicyError`] for something Gemini cannot take and that must not be dropped silently: image
/// or document URLs (the gateway never fetches them), more stop sequences than the API accepts, or
/// a tool result that answers no call.
pub fn build(req: &IrRequest, target: &Target) -> Result<UpstreamRequest, PolicyError> {
    let mut ignored = Policy::collector();
    let mut names = ToolNames::new();
    let mut body = Map::new();

    let (system, contents) = contents(req, target, &mut names, &mut ignored)?;
    if !system.is_empty() {
        body.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": system}]}),
        );
    }
    body.insert("contents".into(), Json::Array(contents));

    if !req.tools.is_empty() {
        let decls: Vec<Json> = req
            .tools
            .iter()
            .map(|t| {
                let mut d = Map::new();
                d.insert("name".into(), json!(names.shorten(&t.name)));
                if let Some(desc) = &t.description {
                    d.insert("description".into(), json!(desc));
                }
                if has_properties(&t.input_schema) {
                    d.insert("parametersJsonSchema".into(), t.input_schema.clone());
                }
                if t.strict == Some(true) {
                    ignored.ignore("strict", "Gemini has no strict schema adherence for tools");
                }
                Json::Object(d)
            })
            .collect();
        body.insert("tools".into(), json!([{"functionDeclarations": decls}]));
        let config = match &req.tool_choice.kind {
            ToolChoiceKind::Auto => None,
            ToolChoiceKind::Required => Some(json!({"mode": "ANY"})),
            ToolChoiceKind::None => Some(json!({"mode": "NONE"})),
            ToolChoiceKind::Named(n) => {
                Some(json!({"mode": "ANY", "allowedFunctionNames": [names.shorten(n)]}))
            }
        };
        if let Some(c) = config {
            body.insert("toolConfig".into(), json!({"functionCallingConfig": c}));
        }
        if !req.tool_choice.parallel {
            ignored.ignore(
                "parallel_tool_calls",
                "Gemini cannot be told to call one tool at a time",
            );
        }
    }

    let mut gen = Map::new();
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
        gen.insert("maxOutputTokens".into(), json!(limit));
    }
    let s = &req.sampling;
    if let Some(t) = s.temperature {
        gen.insert("temperature".into(), json!(t));
    }
    if let Some(p) = s.top_p {
        gen.insert("topP".into(), json!(p));
    }
    if let Some(k) = s.top_k {
        gen.insert("topK".into(), json!(k));
    }
    if let Some(seed) = s.seed {
        gen.insert("seed".into(), json!(seed));
    }
    if let Some(p) = s.frequency_penalty {
        gen.insert("frequencyPenalty".into(), json!(p));
    }
    if let Some(p) = s.presence_penalty {
        gen.insert("presencePenalty".into(), json!(p));
    }
    if !req.stop.is_empty() {
        if req.stop.len() > MAX_STOP_SEQUENCES {
            return Err(reject(
                "stop_sequences",
                format!(
                    "Gemini accepts at most {MAX_STOP_SEQUENCES} stop sequences, got {}",
                    req.stop.len()
                ),
            ));
        }
        gen.insert("stopSequences".into(), json!(req.stop));
    }
    match &req.response_format {
        ResponseFormat::Text => {}
        ResponseFormat::JsonObject => {
            gen.insert("responseMimeType".into(), json!("application/json"));
        }
        ResponseFormat::JsonSchema { schema, .. } => {
            gen.insert("responseMimeType".into(), json!("application/json"));
            gen.insert("responseJsonSchema".into(), schema.clone());
        }
    }
    if let Some(t) = thinking(req, target) {
        gen.insert("thinkingConfig".into(), t);
    }
    if !gen.is_empty() {
        body.insert("generationConfig".into(), Json::Object(gen));
    }
    if req.cache_hints {
        ignored.ignore("cache_control", "Gemini caches prompts implicitly");
    }

    Ok(UpstreamRequest {
        path: path(&target.model, req.stream),
        body: serde_json::to_vec(&Json::Object(body)).expect("a JSON value serialises"),
        stream: req.stream,
        tool_names: names,
        ignored: ignored.into_ignored(),
    })
}

fn has_properties(schema: &Json) -> bool {
    schema
        .get("properties")
        .and_then(Json::as_object)
        .is_some_and(|p| !p.is_empty())
}

fn is_gemini_3(model: &str) -> bool {
    model
        .strip_prefix("models/")
        .unwrap_or(model)
        .starts_with("gemini-3")
}

/// The thinking budget a level maps to on models that take a budget.
fn budget_for(e: Effort) -> u32 {
    match e {
        Effort::Low => 1_024,
        Effort::Medium => 8_192,
        Effort::High => 16_384,
        Effort::XHigh | Effort::Max => 24_576,
    }
}

fn thinking(req: &IrRequest, target: &Target) -> Option<Json> {
    let show = req.reasoning.display != ReasoningDisplay::Omitted;
    let effort = effective_effort(req);
    let gemini3 = is_gemini_3(&target.model);
    let level = |e: Effort| if e >= Effort::High { "high" } else { "low" };
    match req.reasoning.mode {
        ReasoningMode::On { budget_tokens } => {
            let mut t = Map::new();
            t.insert("includeThoughts".into(), json!(show));
            if gemini3 {
                if let Some(e) = effort.or_else(|| budget_tokens.map(Effort::from_budget)) {
                    t.insert("thinkingLevel".into(), json!(level(e)));
                }
            } else if let Some(b) = budget_tokens {
                t.insert("thinkingBudget".into(), json!(b.min(24_576)));
            } else if let Some(e) = effort {
                t.insert("thinkingBudget".into(), json!(budget_for(e)));
            }
            Some(Json::Object(t))
        }
        ReasoningMode::Adaptive => {
            let mut t = Map::new();
            t.insert("includeThoughts".into(), json!(show));
            if gemini3 {
                if let Some(e) = effort {
                    t.insert("thinkingLevel".into(), json!(level(e)));
                }
            } else if let Some(e) = effort {
                t.insert("thinkingBudget".into(), json!(budget_for(e)));
            } else {
                // -1 lets the model decide how much to think.
                t.insert("thinkingBudget".into(), json!(-1));
            }
            Some(Json::Object(t))
        }
        // A model that thinks always (the Pro line) refuses a zero budget, so only others get one.
        ReasoningMode::Off if !gemini3 && !target.model.contains("pro") => {
            Some(json!({"thinkingBudget": 0}))
        }
        ReasoningMode::Off => None,
        ReasoningMode::Unspecified => effort.map(|e| {
            if gemini3 {
                json!({"thinkingLevel": level(e)})
            } else {
                json!({"thinkingBudget": budget_for(e)})
            }
        }),
    }
}

// ---- contents -------------------------------------------------------------------------------

type Contents = Vec<Json>;

fn push(out: &mut Contents, role: &str, mut parts: Vec<Json>) {
    if parts.is_empty() {
        return;
    }
    // Gemini wants roles to alternate, so adjacent turns of one role share a content.
    if let Some(last) = out.last_mut() {
        if last["role"] == role {
            if let Some(existing) = last["parts"].as_array_mut() {
                existing.append(&mut parts);
                return;
            }
        }
    }
    out.push(json!({"role": role, "parts": parts}));
}

fn contents(
    req: &IrRequest,
    target: &Target,
    names: &mut ToolNames,
    ignored: &mut Policy,
) -> Result<(String, Contents), PolicyError> {
    let mut system: Vec<&str> = Vec::new();
    for s in &req.system {
        if s.after_turns > 0 {
            ignored.ignore(
                "system_position",
                "Gemini takes system instructions only at the start; later ones were moved there",
            );
        }
        system.push(&s.text);
    }
    let mut call_names: HashMap<String, String> = HashMap::new();
    let mut out: Contents = Vec::new();
    for turn in &req.turns {
        match turn {
            Turn::User(parts) => push(&mut out, "user", user_parts(parts)?),
            Turn::Assistant(parts) => {
                let ps = assistant_parts(parts, target, names, ignored, &mut call_names);
                push(&mut out, "model", ps);
            }
            Turn::ToolResults(results) => {
                let mut ps = Vec::new();
                for r in results {
                    ps.extend(function_response(r, &call_names, names)?);
                }
                push(&mut out, "user", ps);
            }
        }
    }
    Ok((system.join("\n\n"), out))
}

fn image_part(src: &ImageSource) -> Result<Json, PolicyError> {
    match src {
        ImageSource::Base64 { media_type, data } => {
            Ok(json!({"inlineData": {"mimeType": media_type, "data": data}}))
        }
        ImageSource::Url(_) => Err(reject(
            "image",
            "Gemini needs image bytes: send the image as base64 (airmate never fetches URLs)",
        )),
    }
}

fn user_parts(parts: &[Part]) -> Result<Vec<Json>, PolicyError> {
    let mut out = Vec::new();
    for p in parts {
        match p {
            Part::Text(t) => out.push(json!({"text": t})),
            Part::Image(i) => out.push(image_part(i)?),
            Part::Document(DocumentSource::Text(t)) => out.push(json!({"text": t})),
            Part::Document(DocumentSource::Pdf(data)) => {
                out.push(json!({"inlineData": {"mimeType": "application/pdf", "data": data}}));
            }
            Part::Document(_) => {
                return Err(reject(
                    "document",
                    "Gemini takes documents as text or a base64 PDF, not by URL or file id",
                ))
            }
            Part::ToolCall { .. } | Part::Reasoning { .. } => {}
        }
    }
    Ok(out)
}

fn assistant_parts(
    parts: &[Part],
    target: &Target,
    names: &mut ToolNames,
    ignored: &mut Policy,
    call_names: &mut HashMap<String, String>,
) -> Vec<Json> {
    let mut out: Vec<Json> = Vec::new();
    let mut signature: Option<String> = None;
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
                    continue;
                }
                if let Some(o) = opaque {
                    signature = Some(STANDARD.encode(o));
                }
                if !text.is_empty() {
                    out.push(json!({"text": text, "thought": true}));
                }
            }
            Part::ToolCall { id, name, input } => {
                let short = names.shorten(name);
                call_names.insert(id.clone(), short.clone());
                out.push(json!({"functionCall": {"id": id, "name": short, "args": input}}));
            }
            Part::Text(_) | Part::Image(_) | Part::Document(_) => {}
        }
    }
    // Gemini wants the signature back on the first function call of the step, else on the last
    // part. Gemini 3 insists on one for a call, so without a recorded signature the documented
    // placeholder stands in.
    let first_call = out.iter().position(|p| p.get("functionCall").is_some());
    let signature = match (first_call, signature) {
        (Some(_), None) if is_gemini_3(&target.model) => Some(SKIP_SIGNATURE.to_owned()),
        (_, s) => s,
    };
    if let Some(sig) = signature {
        let at = first_call.or_else(|| out.len().checked_sub(1));
        if let Some(i) = at {
            out[i]["thoughtSignature"] = json!(sig);
        }
    }
    out
}

fn function_response(
    r: &ToolResult,
    call_names: &HashMap<String, String>,
    names: &mut ToolNames,
) -> Result<Vec<Json>, PolicyError> {
    let name = call_names.get(&r.call_id).cloned().ok_or_else(|| {
        reject(
            "tool_result",
            format!(
                "the tool result for `{}` answers no tool call in the conversation",
                r.call_id
            ),
        )
    })?;
    let _ = names;
    let text = r
        .content
        .iter()
        .filter_map(|p| match p {
            Part::Text(t) | Part::Document(DocumentSource::Text(t)) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    let response = if r.is_error {
        json!({"error": text})
    } else {
        json!({"output": text})
    };
    let mut parts =
        vec![json!({"functionResponse": {"id": r.call_id, "name": name, "response": response}})];
    for p in &r.content {
        if let Part::Image(i) = p {
            parts.push(image_part(i)?);
        }
    }
    Ok(parts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;
    use crate::ir::{Origin, Protocol, Reasoning, Sampling, SystemPart, ToolChoice, ToolDef};

    fn target(model: &str) -> Target {
        Target::new("g", Protocol::Gemini, Flavor::Generic, model)
    }

    fn ir() -> IrRequest {
        let mut r = IrRequest::new("claude-x", Protocol::Anthropic);
        r.max_output_tokens = 512;
        r.turns.push(Turn::User(vec![Part::Text("hi".into())]));
        r
    }

    fn built(r: &IrRequest, t: &Target) -> Json {
        serde_json::from_slice(&build(r, t).unwrap().body).unwrap()
    }

    #[test]
    fn the_path_names_the_model_and_the_mode() {
        assert_eq!(
            path("gemini-2.5-pro", false),
            "/v1beta/models/gemini-2.5-pro:generateContent"
        );
        assert_eq!(
            path("models/gemini-2.5-pro", true),
            "/v1beta/models/gemini-2.5-pro:streamGenerateContent?alt=sse"
        );
        let mut r = ir();
        r.stream = true;
        let u = build(&r, &target("gemini-2.5-flash")).unwrap();
        assert!(u.stream && u.path.ends_with("alt=sse"));
    }

    #[test]
    fn a_plain_request_has_contents_system_and_generation_config() {
        let mut r = ir();
        r.system.push(SystemPart {
            text: "be brief".into(),
            after_turns: 0,
            effort: None,
        });
        r.sampling = Sampling {
            temperature: Some(0.2),
            top_p: Some(0.9),
            top_k: Some(40),
            seed: Some(7),
            ..Sampling::default()
        };
        r.stop = vec!["END".into()];
        let b = built(&r, &target("gemini-2.5-pro"));
        assert_eq!(
            b["systemInstruction"],
            json!({"parts": [{"text": "be brief"}]})
        );
        assert_eq!(
            b["contents"],
            json!([{"role": "user", "parts": [{"text": "hi"}]}])
        );
        assert_eq!(
            b["generationConfig"],
            json!({"maxOutputTokens": 512, "temperature": 0.2, "topP": 0.9, "topK": 40, "seed": 7, "stopSequences": ["END"]})
        );
        assert!(b.get("tools").is_none() && b.get("model").is_none());
    }

    #[test]
    fn no_output_limit_from_the_client_sends_none_and_a_cap_applies() {
        let mut r = ir();
        r.max_output_tokens = 0;
        assert!(built(&r, &target("gemini-2.5-pro"))
            .get("generationConfig")
            .is_none());
        let mut t = target("gemini-2.5-pro");
        t.max_output_tokens_cap = Some(100);
        assert_eq!(built(&r, &t)["generationConfig"]["maxOutputTokens"], 100);
        r.max_output_tokens = 5000;
        assert_eq!(built(&r, &t)["generationConfig"]["maxOutputTokens"], 100);
    }

    #[test]
    fn late_system_messages_are_moved_up_and_reported() {
        let mut r = ir();
        r.system.push(SystemPart {
            text: "first".into(),
            after_turns: 0,
            effort: None,
        });
        r.system.push(SystemPart {
            text: "later".into(),
            after_turns: 1,
            effort: None,
        });
        let u = build(&r, &target("gemini-2.5-pro")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        assert_eq!(b["systemInstruction"]["parts"][0]["text"], "first\n\nlater");
        assert_eq!(
            u.ignored
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>(),
            ["system_position"]
        );
    }

    #[test]
    fn tools_calls_and_results_round_trip_with_names_and_ids() {
        let mut r = ir();
        r.tools.push(ToolDef {
            name: "get_weather".into(),
            description: Some("weather".into()),
            input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            strict: Some(true),
        });
        r.tools.push(ToolDef {
            name: "ping".into(),
            description: None,
            input_schema: json!({"type": "object", "properties": {}}),
            strict: None,
        });
        r.turns.push(Turn::Assistant(vec![
            Part::Text("checking".into()),
            Part::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: json!({"city": "Paris"}),
            },
        ]));
        r.turns.push(Turn::ToolResults(vec![ToolResult {
            call_id: "call_1".into(),
            content: vec![Part::Text("sunny".into())],
            is_error: false,
        }]));
        r.turns.push(Turn::User(vec![Part::Text("thanks".into())]));
        let u = build(&r, &target("gemini-2.5-pro")).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        let decls = &b["tools"][0]["functionDeclarations"];
        assert_eq!(
            decls[0]["parametersJsonSchema"]["properties"]["city"]["type"],
            "string"
        );
        assert!(
            decls[1].get("parametersJsonSchema").is_none(),
            "an empty schema is omitted"
        );
        assert_eq!(
            b["contents"][1],
            json!({"role": "model", "parts": [{"text": "checking"}, {"functionCall": {"id": "call_1", "name": "get_weather", "args": {"city": "Paris"}}}]})
        );
        // The tool result and the user's next message share one user content.
        assert_eq!(
            b["contents"][2],
            json!({"role": "user", "parts": [{"functionResponse": {"id": "call_1", "name": "get_weather", "response": {"output": "sunny"}}}, {"text": "thanks"}]})
        );
        assert!(u.ignored.iter().any(|i| i.name == "strict"));
    }

    #[test]
    fn tool_errors_and_images_in_results_are_carried() {
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![Part::ToolCall {
            id: "c".into(),
            name: "t".into(),
            input: json!({}),
        }]));
        r.turns.push(Turn::ToolResults(vec![ToolResult {
            call_id: "c".into(),
            content: vec![
                Part::Text("boom".into()),
                Part::Image(ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }),
            ],
            is_error: true,
        }]));
        let b = built(&r, &target("gemini-2.5-pro"));
        let parts = &b["contents"][2]["parts"];
        assert_eq!(
            parts[0]["functionResponse"]["response"],
            json!({"error": "boom"})
        );
        assert_eq!(
            parts[1],
            json!({"inlineData": {"mimeType": "image/png", "data": "AAAA"}})
        );
    }

    #[test]
    fn a_tool_result_that_answers_nothing_is_refused() {
        let mut r = ir();
        r.turns.push(Turn::ToolResults(vec![ToolResult {
            call_id: "ghost".into(),
            content: vec![],
            is_error: false,
        }]));
        let e = build(&r, &target("gemini-2.5-pro")).unwrap_err();
        assert_eq!(e.field, "tool_result");
    }

    #[test]
    fn tool_choice_maps_to_function_calling_modes() {
        let mut r = ir();
        r.tools.push(ToolDef {
            name: "t".into(),
            description: None,
            input_schema: json!({}),
            strict: None,
        });
        for (kind, want) in [
            (ToolChoiceKind::Auto, None),
            (ToolChoiceKind::Required, Some(json!({"mode": "ANY"}))),
            (ToolChoiceKind::None, Some(json!({"mode": "NONE"}))),
            (
                ToolChoiceKind::Named("t".into()),
                Some(json!({"mode": "ANY", "allowedFunctionNames": ["t"]})),
            ),
        ] {
            r.tool_choice = ToolChoice {
                kind,
                parallel: true,
            };
            let b = built(&r, &target("gemini-2.5-pro"));
            assert_eq!(
                b.get("toolConfig")
                    .map(|c| c["functionCallingConfig"].clone()),
                want
            );
        }
        r.tool_choice = ToolChoice {
            kind: ToolChoiceKind::Auto,
            parallel: false,
        };
        assert!(build(&r, &target("gemini-2.5-pro"))
            .unwrap()
            .ignored
            .iter()
            .any(|i| i.name == "parallel_tool_calls"));
    }

    #[test]
    fn structured_output_uses_a_json_mime_type_and_schema() {
        let mut r = ir();
        r.response_format = ResponseFormat::JsonObject;
        assert_eq!(
            built(&r, &target("gemini-2.5-pro"))["generationConfig"]["responseMimeType"],
            "application/json"
        );
        r.response_format = ResponseFormat::JsonSchema {
            schema: json!({"type": "object"}),
            strict: true,
        };
        let g = &built(&r, &target("gemini-2.5-pro"))["generationConfig"];
        assert_eq!(g["responseJsonSchema"], json!({"type": "object"}));
    }

    #[test]
    fn thinking_follows_the_model_family() {
        let mut r = ir();
        r.max_output_tokens = 0;
        let cfg = |r: &IrRequest, m: &str| {
            built(r, &target(m))["generationConfig"]["thinkingConfig"].clone()
        };
        r.reasoning = Reasoning {
            mode: ReasoningMode::On {
                budget_tokens: Some(4_000),
            },
            ..Reasoning::default()
        };
        assert_eq!(
            cfg(&r, "gemini-2.5-flash"),
            json!({"includeThoughts": true, "thinkingBudget": 4000})
        );
        r.reasoning.mode = ReasoningMode::On {
            budget_tokens: Some(100_000),
        };
        assert_eq!(
            cfg(&r, "gemini-2.5-flash")["thinkingBudget"],
            24_576,
            "clamped to what the line accepts"
        );
        r.reasoning = Reasoning {
            mode: ReasoningMode::Adaptive,
            ..Reasoning::default()
        };
        assert_eq!(
            cfg(&r, "gemini-2.5-flash"),
            json!({"includeThoughts": true, "thinkingBudget": -1})
        );
        r.reasoning = Reasoning {
            mode: ReasoningMode::On {
                budget_tokens: None,
            },
            effort: Some(Effort::High),
            ..Reasoning::default()
        };
        assert_eq!(
            cfg(&r, "gemini-3-pro-preview"),
            json!({"includeThoughts": true, "thinkingLevel": "high"})
        );
        r.reasoning.effort = Some(Effort::Low);
        assert_eq!(cfg(&r, "gemini-3-pro-preview")["thinkingLevel"], "low");
        r.reasoning = Reasoning {
            mode: ReasoningMode::Off,
            ..Reasoning::default()
        };
        assert_eq!(cfg(&r, "gemini-2.5-flash"), json!({"thinkingBudget": 0}));
        assert!(
            built(&r, &target("gemini-2.5-pro"))
                .get("generationConfig")
                .is_none(),
            "Pro cannot switch thinking off"
        );
        r.reasoning = Reasoning {
            mode: ReasoningMode::On {
                budget_tokens: Some(2_000),
            },
            display: ReasoningDisplay::Omitted,
            ..Reasoning::default()
        };
        assert_eq!(cfg(&r, "gemini-2.5-flash")["includeThoughts"], false);
    }

    #[test]
    fn signed_reasoning_from_gemini_returns_with_its_signature_and_foreign_reasoning_is_dropped() {
        let t = target("gemini-3-pro-preview");
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![
            Part::Reasoning {
                text: "thought".into(),
                display: ReasoningDisplay::Summarized,
                origin: Some(t.origin()),
                opaque: Some(vec![1, 2, 3]),
            },
            Part::Reasoning {
                text: "from elsewhere".into(),
                display: ReasoningDisplay::Summarized,
                origin: Some(Origin {
                    upstream: "other".into(),
                    protocol: Protocol::Chat,
                }),
                opaque: None,
            },
            Part::ToolCall {
                id: "c".into(),
                name: "t".into(),
                input: json!({}),
            },
            Part::ToolCall {
                id: "d".into(),
                name: "t".into(),
                input: json!({}),
            },
        ]));
        let u = build(&r, &t).unwrap();
        let b: Json = serde_json::from_slice(&u.body).unwrap();
        let parts = &b["contents"][1]["parts"];
        assert_eq!(parts[0], json!({"text": "thought", "thought": true}));
        assert_eq!(
            parts[1]["thoughtSignature"], "AQID",
            "the first call carries the recorded signature"
        );
        assert!(
            parts[2].get("thoughtSignature").is_none(),
            "later parallel calls carry none"
        );
        assert!(u
            .ignored
            .iter()
            .any(|i| i.name == "thinking_foreign_origin"));
    }

    #[test]
    fn a_signature_with_no_call_rides_on_the_last_part() {
        let t = target("gemini-3-pro-preview");
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![
            Part::Reasoning {
                text: String::new(),
                display: ReasoningDisplay::Omitted,
                origin: Some(t.origin()),
                opaque: Some(vec![9]),
            },
            Part::Text("answer".into()),
        ]));
        let b: Json = serde_json::from_slice(&build(&r, &t).unwrap().body).unwrap();
        assert_eq!(
            b["contents"][1]["parts"],
            json!([{"text": "answer", "thoughtSignature": "CQ=="}])
        );
    }

    #[test]
    fn gemini_3_calls_without_a_recorded_signature_get_the_placeholder() {
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![Part::ToolCall {
            id: "c".into(),
            name: "t".into(),
            input: json!({}),
        }]));
        let b = built(&r, &target("gemini-3-flash-preview"));
        assert_eq!(
            b["contents"][1]["parts"][0]["thoughtSignature"],
            SKIP_SIGNATURE
        );
    }

    #[test]
    fn older_models_get_no_signature_placeholder() {
        let mut r = ir();
        r.turns.push(Turn::Assistant(vec![Part::ToolCall {
            id: "c".into(),
            name: "t".into(),
            input: json!({}),
        }]));
        let b = built(&r, &target("gemini-2.5-pro"));
        assert!(b["contents"][1]["parts"][0]
            .get("thoughtSignature")
            .is_none());
    }

    #[test]
    fn images_must_be_bytes_and_documents_text_or_pdf() {
        let mut r = ir();
        r.turns[0] = Turn::User(vec![Part::Image(ImageSource::Url(
            "https://x/y.png".into(),
        ))]);
        assert_eq!(
            build(&r, &target("gemini-2.5-pro")).unwrap_err().field,
            "image"
        );
        r.turns[0] = Turn::User(vec![Part::Document(DocumentSource::Url(
            "https://x/y.pdf".into(),
        ))]);
        assert_eq!(
            build(&r, &target("gemini-2.5-pro")).unwrap_err().field,
            "document"
        );
        r.turns[0] = Turn::User(vec![
            Part::Document(DocumentSource::Pdf("JVBER".into())),
            Part::Document(DocumentSource::Text("notes".into())),
        ]);
        let b = built(&r, &target("gemini-2.5-pro"));
        assert_eq!(
            b["contents"][0]["parts"][0]["inlineData"]["mimeType"],
            "application/pdf"
        );
        assert_eq!(b["contents"][0]["parts"][1]["text"], "notes");
    }

    #[test]
    fn too_many_stop_sequences_are_refused_not_truncated() {
        let mut r = ir();
        r.stop = (0..6).map(|i| format!("s{i}")).collect();
        let e = build(&r, &target("gemini-2.5-pro")).unwrap_err();
        assert_eq!(e.field, "stop_sequences");
    }

    #[test]
    fn cache_hints_are_reported_as_ignored() {
        let mut r = ir();
        r.cache_hints = true;
        assert!(build(&r, &target("gemini-2.5-pro"))
            .unwrap()
            .ignored
            .iter()
            .any(|i| i.name == "cache_control"));
    }
}
