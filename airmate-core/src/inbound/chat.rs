//! The OpenAI Chat Completions inbound adapter (`POST /v1/chat/completions`).
//!
//! Every request field, message key, content part and tool field is looked up in the policy table
//! ([`crate::policy::chat::TABLE`]); the parser only handles what the table says is translated.

use serde_json::{Map, Value as Json};

use super::{
    invalid, parse_object, want_array, want_bool, want_f64, want_i64, want_object, want_str,
    want_u32, InboundCtx,
};
use crate::ir::{
    Effort, ImageSource, IrRequest, Part, Protocol, Reasoning, ReasoningDisplay, ReasoningMode,
    ResponseFormat, SystemPart, ToolChoiceKind, ToolDef, ToolResult, Turn,
};
use crate::policy::{chat::TABLE, Decision, Group, Parsed, Policy, PolicyError};

/// Parses a Chat Completions request body.
///
/// # Errors
///
/// A [`PolicyError`] naming the field when the body is malformed, a field is rejected, or a field
/// is unknown and `unknown_fields` is `reject`.
pub fn parse_request(body: &[u8], ctx: &InboundCtx<'_>) -> Result<Parsed<IrRequest>, PolicyError> {
    let obj = parse_object(body)?;
    let mut p = Parser {
        policy: Policy::new(TABLE, ctx.policy),
    };
    let ir = p.request(&obj)?;
    Ok(Parsed {
        value: ir,
        ignored: p.policy.into_ignored(),
    })
}

struct Parser {
    policy: Policy,
}

/// Parses a data URI or URL into an image source.
fn image_source(url: &str) -> ImageSource {
    if let Some(rest) = url.strip_prefix("data:") {
        if let Some((meta, data)) = rest.split_once(',') {
            if let Some(media_type) = meta.strip_suffix(";base64") {
                return ImageSource::Base64 {
                    media_type: media_type.to_owned(),
                    data: data.to_owned(),
                };
            }
        }
    }
    ImageSource::Url(url.to_owned())
}

impl Parser {
    fn request(&mut self, obj: &Map<String, Json>) -> Result<IrRequest, PolicyError> {
        let mut ir = IrRequest::new(String::new(), Protocol::Chat);
        let mut have_model = false;
        let mut have_messages = false;
        let mut max_completion: Option<u32> = None;
        let mut max_tokens: Option<u32> = None;
        let mut system: Vec<SystemPart> = Vec::new();
        let mut effort_mode: Option<ReasoningMode> = None;

        for (key, value) in obj {
            if self.policy.check(Group::Request, key)? == Decision::Skip {
                continue;
            }
            match key.as_str() {
                "model" => {
                    ir.model = want_str(value, "model")?.to_owned();
                    have_model = true;
                }
                "messages" => {
                    self.messages(want_array(value, "messages")?, &mut ir, &mut system)?;
                    have_messages = true;
                }
                "max_tokens" => max_tokens = Some(want_u32(value, "max_tokens")?),
                "max_completion_tokens" => {
                    max_completion = Some(want_u32(value, "max_completion_tokens")?)
                }
                "temperature" => ir.sampling.temperature = Some(want_f64(value, "temperature")?),
                "top_p" => ir.sampling.top_p = Some(want_f64(value, "top_p")?),
                "top_k" => ir.sampling.top_k = Some(want_u32(value, "top_k")?),
                "seed" => ir.sampling.seed = Some(want_i64(value, "seed")?),
                "frequency_penalty" => {
                    ir.sampling.frequency_penalty = Some(want_f64(value, "frequency_penalty")?)
                }
                "presence_penalty" => {
                    ir.sampling.presence_penalty = Some(want_f64(value, "presence_penalty")?)
                }
                "stream" => ir.stream = want_bool(value, "stream")?,
                "stream_options" => {
                    let o = want_object(value, "stream_options")?;
                    for (k, v) in o {
                        match k.as_str() {
                            "include_usage" => {
                                ir.extensions.stream_usage =
                                    want_bool(v, "stream_options.include_usage")?
                            }
                            "include_obfuscation" => {
                                want_bool(v, "stream_options.include_obfuscation")?;
                                self.policy.ignore(
                                    "stream_options.include_obfuscation",
                                    "no upstream equivalent",
                                );
                            }
                            other => {
                                return Err(invalid(
                                    format!("stream_options.{other}"),
                                    format!("`stream_options.{other}` is not known to airmate"),
                                ));
                            }
                        }
                    }
                }
                "stop" => {
                    ir.stop = match value {
                        Json::String(s) => vec![s.clone()],
                        Json::Array(a) => a
                            .iter()
                            .enumerate()
                            .map(|(i, s)| want_str(s, &format!("stop[{i}]")).map(str::to_owned))
                            .collect::<Result<_, _>>()?,
                        Json::Null => Vec::new(),
                        _ => {
                            return Err(invalid(
                                "stop",
                                "`stop` must be a string or an array of strings",
                            ))
                        }
                    };
                }
                "tools" => ir.tools = self.tools(want_array(value, "tools")?)?,
                "tool_choice" => ir.tool_choice.kind = self.tool_choice(value)?,
                "parallel_tool_calls" => {
                    ir.tool_choice.parallel = want_bool(value, "parallel_tool_calls")?
                }
                "response_format" => ir.response_format = self.response_format(value)?,
                "reasoning_effort" => {
                    let e = want_str(value, "reasoning_effort")?;
                    match e {
                        "none" => effort_mode = Some(ReasoningMode::Off),
                        "minimal" => ir.reasoning.effort = Some(Effort::Low),
                        other => {
                            ir.reasoning.effort = Some(Effort::parse(other).ok_or_else(|| {
                                invalid("reasoning_effort", format!("unknown effort `{other}`; expected none, minimal, low, medium, high, xhigh or max"))
                            })?);
                        }
                    }
                }
                "thinking" => ir.reasoning = self.thinking(value, ir.reasoning.effort)?,
                "prompt_cache_key" => {
                    want_str(value, "prompt_cache_key")?;
                    ir.cache_hints = true;
                }
                "n" => {
                    if want_u32(value, "n")? != 1 {
                        return Err(invalid("n", "`n` must be 1: airmate returns one choice"));
                    }
                }
                other => {
                    unreachable!("table says `{other}` is translated but the parser has no case")
                }
            }
        }
        if !have_model {
            return Err(invalid("model", "`model` is required"));
        }
        if !have_messages {
            return Err(invalid("messages", "`messages` is required"));
        }
        if let Some(m) = effort_mode {
            ir.reasoning.mode = m;
        }
        // `max_completion_tokens` is the current name and wins over the deprecated `max_tokens`.
        ir.max_output_tokens = max_completion.or(max_tokens).unwrap_or(0);
        if ir.max_output_tokens == 0 && (max_completion.is_some() || max_tokens.is_some()) {
            return Err(invalid(
                "max_tokens",
                "the output token limit must be greater than 0",
            ));
        }
        ir.system = system;
        Ok(ir)
    }

    fn thinking(&mut self, value: &Json, effort: Option<Effort>) -> Result<Reasoning, PolicyError> {
        let obj = want_object(value, "thinking")?;
        let mut ty = None;
        let mut budget = None;
        for (k, v) in obj {
            if self.policy.check(Group::ThinkingField, k)? == Decision::Skip {
                continue;
            }
            match k.as_str() {
                "type" => ty = Some(want_str(v, "thinking.type")?),
                "budget_tokens" => budget = Some(want_u32(v, "thinking.budget_tokens")?),
                other => unreachable!("table says thinking field `{other}` is translated"),
            }
        }
        let mode = match ty {
            Some("enabled") => ReasoningMode::On {
                budget_tokens: budget,
            },
            Some("adaptive") => ReasoningMode::Adaptive,
            Some("disabled") => ReasoningMode::Off,
            Some(other) => {
                return Err(invalid(
                    "thinking.type",
                    format!("unknown thinking type `{other}`"),
                ))
            }
            None => return Err(invalid("thinking.type", "`type` is required")),
        };
        Ok(Reasoning {
            mode,
            effort,
            display: ReasoningDisplay::Summarized,
        })
    }

    fn response_format(&mut self, value: &Json) -> Result<ResponseFormat, PolicyError> {
        let obj = want_object(value, "response_format")?;
        match obj.get("type").and_then(Json::as_str) {
            Some("text") => Ok(ResponseFormat::Text),
            Some("json_object") => Ok(ResponseFormat::JsonObject),
            Some("json_schema") => {
                let js = obj.get("json_schema").ok_or_else(|| {
                    invalid("response_format.json_schema", "`json_schema` is required")
                })?;
                let js = want_object(js, "response_format.json_schema")?;
                let schema = js.get("schema").ok_or_else(|| {
                    invalid("response_format.json_schema.schema", "`schema` is required")
                })?;
                Ok(ResponseFormat::JsonSchema {
                    schema: schema.clone(),
                    strict: js.get("strict").and_then(Json::as_bool).unwrap_or(false),
                })
            }
            _ => Err(invalid(
                "response_format.type",
                "`type` must be `text`, `json_object` or `json_schema`",
            )),
        }
    }

    // ---- tools ------------------------------------------------------------------------------

    fn tools(&mut self, tools: &[Json]) -> Result<Vec<ToolDef>, PolicyError> {
        let mut out = Vec::new();
        for (i, t) in tools.iter().enumerate() {
            let path = format!("tools[{i}]");
            let obj = want_object(t, &path)?;
            let mut function = None;
            for (k, v) in obj {
                if self.policy.check(Group::ToolField, k)? == Decision::Skip {
                    continue;
                }
                match k.as_str() {
                    "type" => {
                        let ty = want_str(v, &format!("{path}.type"))?;
                        if ty != "function" {
                            return Err(invalid(
                                format!("{path}.type"),
                                format!("tool type `{ty}` is not supported by airmate; only `function` tools are"),
                            ));
                        }
                    }
                    "function" => function = Some(want_object(v, &format!("{path}.function"))?),
                    other => unreachable!("table says tool field `{other}` is translated"),
                }
            }
            let f = function
                .ok_or_else(|| invalid(format!("{path}.function"), "`function` is required"))?;
            let mut name = None;
            let mut description = None;
            let mut schema = None;
            let mut strict = None;
            for (k, v) in f {
                if self
                    .policy
                    .check(Group::ToolField, &format!("function.{k}"))?
                    == Decision::Skip
                {
                    continue;
                }
                let fp = format!("{path}.function.{k}");
                match k.as_str() {
                    "name" => name = Some(want_str(v, &fp)?.to_owned()),
                    "description" => description = Some(want_str(v, &fp)?.to_owned()),
                    "parameters" => {
                        want_object(v, &fp)?;
                        schema = Some(v.clone());
                    }
                    "strict" => strict = v.as_bool(),
                    other => unreachable!("table says function field `{other}` is translated"),
                }
            }
            out.push(ToolDef {
                name: name.ok_or_else(|| {
                    invalid(format!("{path}.function.name"), "`name` is required")
                })?,
                description,
                input_schema: schema
                    .unwrap_or_else(|| serde_json::json!({"type": "object", "properties": {}})),
                strict,
            });
        }
        Ok(out)
    }

    fn tool_choice(&mut self, value: &Json) -> Result<ToolChoiceKind, PolicyError> {
        match value {
            Json::String(s) => match s.as_str() {
                "none" => Ok(ToolChoiceKind::None),
                "auto" => Ok(ToolChoiceKind::Auto),
                "required" => Ok(ToolChoiceKind::Required),
                other => Err(invalid(
                    "tool_choice",
                    format!("unknown tool_choice `{other}`"),
                )),
            },
            Json::Object(o) => {
                let mut name = None;
                for (k, v) in o {
                    if self.policy.check(Group::ToolChoiceField, k)? == Decision::Skip {
                        continue;
                    }
                    match k.as_str() {
                        "type" => {
                            let ty = want_str(v, "tool_choice.type")?;
                            if ty != "function" {
                                return Err(invalid(
                                    "tool_choice.type",
                                    format!(
                                        "tool_choice type `{ty}` is not supported; use `function`"
                                    ),
                                ));
                            }
                        }
                        "function" => {
                            let f = want_object(v, "tool_choice.function")?;
                            for fk in f.keys() {
                                self.policy
                                    .check(Group::ToolChoiceField, &format!("function.{fk}"))?;
                            }
                            name = f.get("name").and_then(Json::as_str).map(str::to_owned);
                        }
                        other => {
                            unreachable!("table says tool_choice field `{other}` is translated")
                        }
                    }
                }
                name.map(ToolChoiceKind::Named).ok_or_else(|| {
                    invalid("tool_choice.function.name", "`function.name` is required")
                })
            }
            _ => Err(invalid(
                "tool_choice",
                "`tool_choice` must be a string or an object",
            )),
        }
    }

    // ---- messages ---------------------------------------------------------------------------

    fn messages(
        &mut self,
        messages: &[Json],
        ir: &mut IrRequest,
        system: &mut Vec<SystemPart>,
    ) -> Result<(), PolicyError> {
        for (i, m) in messages.iter().enumerate() {
            let path = format!("messages[{i}]");
            let obj = want_object(m, &path)?;
            let mut role = None;
            let mut content: Option<&Json> = None;
            let mut tool_calls: Option<&Json> = None;
            let mut tool_call_id = None;
            let mut refusal = None;
            for (k, v) in obj {
                if self.policy.check(Group::MessageKey, k)? == Decision::Skip {
                    continue;
                }
                let kp = format!("{path}.{k}");
                match k.as_str() {
                    "role" => role = Some(want_str(v, &kp)?),
                    "content" => content = Some(v),
                    "tool_calls" => tool_calls = Some(v),
                    "tool_call_id" => tool_call_id = Some(want_str(v, &kp)?.to_owned()),
                    "refusal" => refusal = v.as_str().map(str::to_owned),
                    other => unreachable!("table says message key `{other}` is translated"),
                }
            }
            let role = role.ok_or_else(|| invalid(format!("{path}.role"), "`role` is required"))?;
            match role {
                "system" | "developer" => {
                    let parts = self.parts(content, &path, false)?;
                    for part in parts {
                        if let Part::Text(text) = part {
                            system.push(SystemPart {
                                text,
                                after_turns: ir.turns.len(),
                                effort: None,
                            });
                        }
                    }
                }
                "user" => {
                    let parts = self.parts(content, &path, true)?;
                    ir.turns.push(Turn::User(parts));
                }
                "assistant" => {
                    let mut parts = self.parts(content, &path, false)?;
                    if let Some(r) = refusal.filter(|r| !r.is_empty()) {
                        parts.push(Part::Text(r));
                    }
                    if let Some(tc) = tool_calls {
                        for (j, c) in want_array(tc, &format!("{path}.tool_calls"))?
                            .iter()
                            .enumerate()
                        {
                            parts.push(self.tool_call(c, &format!("{path}.tool_calls[{j}]"))?);
                        }
                    }
                    ir.turns.push(Turn::Assistant(parts));
                }
                "tool" => {
                    let call_id = tool_call_id.ok_or_else(|| {
                        invalid(
                            format!("{path}.tool_call_id"),
                            "`tool_call_id` is required for a tool message",
                        )
                    })?;
                    let result = ToolResult {
                        call_id,
                        content: self.parts(content, &path, false)?,
                        is_error: false,
                    };
                    match ir.turns.last_mut() {
                        Some(Turn::ToolResults(rs)) => rs.push(result),
                        _ => ir.turns.push(Turn::ToolResults(vec![result])),
                    }
                }
                "function" => {
                    return Err(invalid(
                        format!("{path}.role"),
                        "the `function` role is deprecated by OpenAI; use `tool` messages",
                    ));
                }
                other => {
                    return Err(invalid(
                        format!("{path}.role"),
                        format!("unknown role `{other}`; expected system, developer, user, assistant or tool"),
                    ));
                }
            }
        }
        Ok(())
    }

    fn tool_call(&mut self, c: &Json, path: &str) -> Result<Part, PolicyError> {
        let obj = want_object(c, path)?;
        let id = want_str(
            obj.get("id")
                .ok_or_else(|| invalid(format!("{path}.id"), "`id` is required"))?,
            &format!("{path}.id"),
        )?;
        let f = want_object(
            obj.get("function")
                .ok_or_else(|| invalid(format!("{path}.function"), "`function` is required"))?,
            &format!("{path}.function"),
        )?;
        let name = want_str(
            f.get("name")
                .ok_or_else(|| invalid(format!("{path}.function.name"), "`name` is required"))?,
            &format!("{path}.function.name"),
        )?;
        let args = f.get("arguments").and_then(Json::as_str).unwrap_or("");
        let input = if args.trim().is_empty() {
            serde_json::json!({})
        } else {
            serde_json::from_str(args).map_err(|e| {
                invalid(
                    format!("{path}.function.arguments"),
                    format!("the arguments of `{name}` are not valid JSON: {e}"),
                )
            })?
        };
        Ok(Part::ToolCall {
            id: id.to_owned(),
            name: name.to_owned(),
            input,
        })
    }

    /// Content as parts: a string, or a list of typed parts. `images` says whether image parts are
    /// allowed (user messages only).
    fn parts(
        &mut self,
        content: Option<&Json>,
        path: &str,
        images: bool,
    ) -> Result<Vec<Part>, PolicyError> {
        match content {
            None | Some(Json::Null) => Ok(Vec::new()),
            Some(Json::String(s)) => Ok(vec![Part::Text(s.clone())]),
            Some(Json::Array(parts)) => {
                let mut out = Vec::new();
                for (i, p) in parts.iter().enumerate() {
                    let pp = format!("{path}.content[{i}]");
                    let obj = want_object(p, &pp)?;
                    let ty = want_str(
                        obj.get("type").ok_or_else(|| {
                            invalid(format!("{pp}.type"), "a content part needs a `type`")
                        })?,
                        &format!("{pp}.type"),
                    )?;
                    if self.policy.check(Group::ContentBlock, ty)? == Decision::Skip {
                        continue;
                    }
                    for k in obj.keys().filter(|k| *k != "type") {
                        self.policy
                            .check(Group::BlockField, &format!("{ty}.{k}"))
                            .map_err(|mut e| {
                                e.field = format!("{pp}.{k}");
                                e
                            })?;
                    }
                    match ty {
                        "text" => out.push(Part::Text(
                            want_str(
                                obj.get("text").ok_or_else(|| {
                                    invalid(format!("{pp}.text"), "`text` is required")
                                })?,
                                &format!("{pp}.text"),
                            )?
                            .to_owned(),
                        )),
                        "refusal" => out.push(Part::Text(
                            want_str(
                                obj.get("refusal").ok_or_else(|| {
                                    invalid(format!("{pp}.refusal"), "`refusal` is required")
                                })?,
                                &format!("{pp}.refusal"),
                            )?
                            .to_owned(),
                        )),
                        "image_url" => {
                            if !images {
                                return Err(invalid(
                                    &pp,
                                    "images are only allowed in user messages",
                                ));
                            }
                            let iu = want_object(
                                obj.get("image_url").ok_or_else(|| {
                                    invalid(format!("{pp}.image_url"), "`image_url` is required")
                                })?,
                                &format!("{pp}.image_url"),
                            )?;
                            for k in iu.keys() {
                                self.policy.check(
                                    Group::BlockField,
                                    &format!("image_url.image_url.{k}"),
                                )?;
                            }
                            let url = want_str(
                                iu.get("url").ok_or_else(|| {
                                    invalid(format!("{pp}.image_url.url"), "`url` is required")
                                })?,
                                &format!("{pp}.image_url.url"),
                            )?;
                            out.push(Part::Image(image_source(url)));
                        }
                        other => unreachable!("table says content part `{other}` is translated"),
                    }
                }
                Ok(out)
            }
            Some(_) => Err(invalid(
                format!("{path}.content"),
                "`content` must be a string or an array of parts",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::ToolChoice;
    use crate::policy::{PolicyConfig, UnknownFields};
    use serde_json::json;

    fn parse(v: Json) -> Result<Parsed<IrRequest>, PolicyError> {
        parse_request(v.to_string().as_bytes(), &InboundCtx::default())
    }

    fn ok(v: Json) -> Parsed<IrRequest> {
        parse(v).expect("parses")
    }

    fn base() -> Json {
        json!({"model": "gpt-4.1", "messages": [{"role": "user", "content": "hi"}]})
    }

    fn with(extra: Json) -> Json {
        let mut b = base();
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    }

    #[test]
    fn parses_a_minimal_request_with_no_output_limit() {
        let p = ok(base());
        assert_eq!(p.value.model, "gpt-4.1");
        assert_eq!(p.value.max_output_tokens, 0, "unset");
        assert_eq!(
            p.value.turns,
            vec![Turn::User(vec![Part::Text("hi".into())])]
        );
        assert!(p.ignored.is_empty() && !p.value.stream && !p.value.extensions.stream_usage);
        assert_eq!(p.value.extensions.inbound, Protocol::Chat);
    }

    #[test]
    fn model_and_messages_are_required_and_the_body_must_be_an_object() {
        for key in ["model", "messages"] {
            let mut b = base();
            b.as_object_mut().unwrap().remove(key);
            assert_eq!(parse(b).unwrap_err().field, key);
        }
        assert!(parse_request(b"[]", &InboundCtx::default()).is_err());
        assert!(parse_request(b"x", &InboundCtx::default()).is_err());
    }

    #[test]
    fn the_output_limit_prefers_max_completion_tokens() {
        assert_eq!(
            ok(with(json!({"max_tokens": 10}))).value.max_output_tokens,
            10
        );
        assert_eq!(
            ok(with(json!({"max_completion_tokens": 20})))
                .value
                .max_output_tokens,
            20
        );
        assert_eq!(
            ok(with(json!({"max_tokens": 10, "max_completion_tokens": 20})))
                .value
                .max_output_tokens,
            20
        );
        assert!(parse(with(json!({"max_tokens": 0}))).is_err());
        assert!(parse(with(json!({"max_tokens": -1}))).is_err());
    }

    #[test]
    fn sampling_stop_and_stream_options() {
        let p = ok(with(
            json!({"temperature": 0.2, "top_p": 0.9, "top_k": 5, "seed": 7, "frequency_penalty": 0.1, "presence_penalty": 0.2,
            "stream": true, "stream_options": {"include_usage": true}, "stop": ["a", "b"]}),
        ));
        let ir = p.value;
        assert_eq!(
            (
                ir.sampling.temperature,
                ir.sampling.top_p,
                ir.sampling.top_k,
                ir.sampling.seed
            ),
            (Some(0.2), Some(0.9), Some(5), Some(7))
        );
        assert_eq!(
            (ir.sampling.frequency_penalty, ir.sampling.presence_penalty),
            (Some(0.1), Some(0.2))
        );
        assert!(ir.stream && ir.extensions.stream_usage);
        assert_eq!(ir.stop, vec!["a".to_owned(), "b".to_owned()]);
        assert_eq!(
            ok(with(json!({"stop": "END"}))).value.stop,
            vec!["END".to_owned()]
        );
        assert!(ok(with(json!({"stop": null}))).value.stop.is_empty());
        assert!(parse(with(json!({"stop": 3}))).is_err());
        assert!(parse(with(json!({"stream_options": {"mystery": true}}))).is_err());
        let p = ok(with(
            json!({"stream_options": {"include_obfuscation": false}}),
        ));
        assert!(p
            .ignored
            .iter()
            .any(|i| i.name == "stream_options.include_obfuscation"));
    }

    #[test]
    fn behaviour_changing_fields_are_rejected_by_name() {
        for field in [
            "logit_bias",
            "logprobs",
            "top_logprobs",
            "modalities",
            "audio",
            "web_search_options",
            "functions",
            "function_call",
        ] {
            let err = parse(with(json!({ field: {} }))).unwrap_err();
            assert_eq!(err.field, field);
            assert!(err.message.contains(field));
        }
        let err = parse(with(json!({"n": 2}))).unwrap_err();
        assert!(err.message.contains("`n` must be 1"));
        assert!(parse(with(json!({"n": 1}))).is_ok());
    }

    #[test]
    fn harmless_fields_are_ignored_and_reported() {
        let p = ok(with(
            json!({"user": "u", "metadata": {}, "store": true, "service_tier": "auto", "safety_identifier": "x", "verbosity": "low", "prediction": {}}),
        ));
        let names: Vec<_> = p.ignored.iter().map(|i| i.name.as_str()).collect();
        for n in [
            "user",
            "metadata",
            "store",
            "service_tier",
            "safety_identifier",
            "verbosity",
            "prediction",
        ] {
            assert!(names.contains(&n), "{n}");
        }
    }

    #[test]
    fn unknown_fields_are_rejected_or_warned() {
        assert!(parse(with(json!({"shiny": 1})))
            .unwrap_err()
            .message
            .contains("not known to airmate"));
        let ctx = InboundCtx {
            policy: PolicyConfig {
                unknown_fields: UnknownFields::Warn,
            },
            ..InboundCtx::default()
        };
        let p = parse_request(with(json!({"shiny": 1})).to_string().as_bytes(), &ctx).unwrap();
        assert_eq!(p.ignored[0].name, "shiny");
    }

    #[test]
    fn system_and_developer_messages_keep_their_position() {
        let p = ok(json!({"model": "m", "messages": [
            {"role": "system", "content": "lead"},
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": "a"},
            {"role": "developer", "content": [{"type": "text", "text": "mid"}]},
            {"role": "user", "content": "q2"}]}));
        let s = &p.value.system;
        assert_eq!((s[0].text.as_str(), s[0].after_turns), ("lead", 0));
        assert_eq!((s[1].text.as_str(), s[1].after_turns), ("mid", 2));
        assert_eq!(p.value.turns.len(), 3);
    }

    #[test]
    fn user_content_parts_and_images() {
        let p = ok(
            json!({"model": "m", "messages": [{"role": "user", "content": [
            {"type": "text", "text": "look"},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA", "detail": "high"}},
            {"type": "image_url", "image_url": {"url": "https://x/y.png"}}]}]}),
        );
        assert_eq!(
            p.value.turns[0],
            Turn::User(vec![
                Part::Text("look".into()),
                Part::Image(ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "AAAA".into()
                }),
                Part::Image(ImageSource::Url("https://x/y.png".into())),
            ])
        );
        assert!(p
            .ignored
            .iter()
            .any(|i| i.name == "image_url.image_url.detail"));
        for bad in [
            json!({"type": "input_audio", "input_audio": {}}),
            json!({"type": "file", "file": {}}),
            json!({"type": "hologram"}),
            json!({"text": "no type"}),
        ] {
            assert!(
                parse(json!({"model": "m", "messages": [{"role": "user", "content": [bad]}]}))
                    .is_err()
            );
        }
        assert!(parse(json!({"model": "m", "messages": [{"role": "system", "content": [{"type": "image_url", "image_url": {"url": "u"}}]}]})).is_err());
        assert!(
            parse(json!({"model": "m", "messages": [{"role": "user", "content": 5}]})).is_err()
        );
    }

    #[test]
    fn an_assistant_turn_with_tool_calls_and_the_tool_results_that_answer_it() {
        let p = ok(json!({"model": "m", "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "c1", "type": "function", "function": {"name": "read", "arguments": "{\"p\":1}"}},
                {"id": "c2", "type": "function", "function": {"name": "ls", "arguments": ""}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "file contents"},
            {"role": "tool", "tool_call_id": "c2", "content": [{"type": "text", "text": "listing"}]},
            {"role": "user", "content": "thanks"}]}));
        assert_eq!(p.value.turns.len(), 4);
        match &p.value.turns[1] {
            Turn::Assistant(parts) => {
                assert_eq!(
                    parts[0],
                    Part::ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        input: json!({"p": 1})
                    }
                );
                assert_eq!(
                    parts[1],
                    Part::ToolCall {
                        id: "c2".into(),
                        name: "ls".into(),
                        input: json!({})
                    }
                );
            }
            other => panic!("{other:?}"),
        }
        match &p.value.turns[2] {
            Turn::ToolResults(rs) => {
                assert_eq!(
                    rs.len(),
                    2,
                    "consecutive tool messages are one results turn"
                );
                assert_eq!(rs[0].call_id, "c1");
                assert_eq!(rs[1].content, vec![Part::Text("listing".into())]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn malformed_messages_and_tool_calls_are_rejected() {
        let msg = |m: Json| parse(json!({"model": "m", "messages": [m]}));
        assert_eq!(
            msg(json!({"content": "x"})).unwrap_err().field,
            "messages[0].role"
        );
        assert!(msg(json!({"role": "robot", "content": "x"}))
            .unwrap_err()
            .message
            .contains("unknown role"));
        assert!(
            msg(json!({"role": "function", "name": "f", "content": "x"}))
                .unwrap_err()
                .message
                .contains("deprecated")
        );
        assert!(msg(json!({"role": "tool", "content": "x"}))
            .unwrap_err()
            .field
            .contains("tool_call_id"));
        assert!(msg(json!({"role": "assistant", "tool_calls": [{"id": "c", "function": {"name": "n", "arguments": "{bad"}}]})).unwrap_err().message.contains("not valid JSON"));
        assert!(
            msg(json!({"role": "assistant", "tool_calls": [{"function": {"name": "n"}}]})).is_err()
        );
        assert!(msg(json!({"role": "assistant", "tool_calls": "x"})).is_err());
        assert!(msg(json!({"role": "user", "content": "x", "audio": {}})).is_err());
        assert!(parse(json!({"model": "m", "messages": ["x"]})).is_err());
        assert!(msg(json!({"role": "assistant", "function_call": {}})).is_err());
    }

    #[test]
    fn message_extras_are_ignored_and_reported() {
        let p = ok(
            json!({"model": "m", "messages": [{"role": "user", "name": "bob", "content": "x"}, {"role": "assistant", "content": "a", "reasoning_content": "thoughts", "annotations": []}]}),
        );
        let names: Vec<_> = p.ignored.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"name")
                && names.contains(&"reasoning_content")
                && names.contains(&"annotations")
        );
        let p = ok(
            json!({"model": "m", "messages": [{"role": "assistant", "content": "a", "refusal": "no"}]}),
        );
        assert_eq!(
            p.value.turns[0],
            Turn::Assistant(vec![Part::Text("a".into()), Part::Text("no".into())])
        );
    }

    #[test]
    fn tools_and_tool_choice() {
        let p = ok(with(json!({"tools": [
            {"type": "function", "function": {"name": "read", "description": "d", "parameters": {"type": "object"}, "strict": true}},
            {"type": "function", "function": {"name": "bare"}}],
            "tool_choice": {"type": "function", "function": {"name": "read"}}, "parallel_tool_calls": false})));
        assert_eq!(p.value.tools[0].strict, Some(true));
        assert_eq!(
            p.value.tools[1].input_schema,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            p.value.tool_choice,
            ToolChoice {
                kind: ToolChoiceKind::Named("read".into()),
                parallel: false
            }
        );
        for (v, want) in [
            ("none", ToolChoiceKind::None),
            ("auto", ToolChoiceKind::Auto),
            ("required", ToolChoiceKind::Required),
        ] {
            assert_eq!(
                ok(with(json!({"tool_choice": v}))).value.tool_choice.kind,
                want
            );
        }
        assert!(parse(with(json!({"tool_choice": "sometimes"}))).is_err());
        assert!(parse(with(json!({"tool_choice": {"type": "allowed_tools"}}))).is_err());
        assert!(parse(with(json!({"tool_choice": {"type": "function"}}))).is_err());
        assert!(parse(with(json!({"tool_choice": 3}))).is_err());
        assert!(parse(with(json!({"tools": [{"type": "custom"}]})))
            .unwrap_err()
            .message
            .contains("only `function`"));
        assert!(parse(with(json!({"tools": [{"type": "function"}]}))).is_err());
        assert!(parse(with(
            json!({"tools": [{"type": "function", "function": {"description": "x"}}]})
        ))
        .is_err());
        assert!(parse(with(
            json!({"tools": [{"type": "function", "function": {"name": "n", "weird": 1}}]})
        ))
        .is_err());
        assert!(parse(with(
            json!({"tools": [{"type": "function", "function": {"name": "n", "parameters": 3}}]})
        ))
        .is_err());
    }

    #[test]
    fn response_formats() {
        assert_eq!(
            ok(with(json!({"response_format": {"type": "text"}})))
                .value
                .response_format,
            ResponseFormat::Text
        );
        assert_eq!(
            ok(with(json!({"response_format": {"type": "json_object"}})))
                .value
                .response_format,
            ResponseFormat::JsonObject
        );
        let p = ok(with(
            json!({"response_format": {"type": "json_schema", "json_schema": {"name": "x", "schema": {"type": "object"}, "strict": true}}}),
        ));
        assert_eq!(
            p.value.response_format,
            ResponseFormat::JsonSchema {
                schema: json!({"type": "object"}),
                strict: true
            }
        );
        assert!(parse(with(json!({"response_format": {"type": "xml"}}))).is_err());
        assert!(parse(with(json!({"response_format": {"type": "json_schema"}}))).is_err());
        assert!(parse(with(
            json!({"response_format": {"type": "json_schema", "json_schema": {"name": "x"}}})
        ))
        .is_err());
    }

    #[test]
    fn reasoning_effort_and_the_thinking_extension() {
        let eff = |v: &str| ok(with(json!({"reasoning_effort": v}))).value.reasoning;
        assert_eq!(eff("high").effort, Some(Effort::High));
        assert_eq!(eff("xhigh").effort, Some(Effort::XHigh));
        assert_eq!(eff("minimal").effort, Some(Effort::Low));
        assert_eq!(eff("none").mode, ReasoningMode::Off);
        assert!(parse(with(json!({"reasoning_effort": "extreme"}))).is_err());
        let th = |v: Json| ok(with(json!({"thinking": v}))).value.reasoning;
        assert_eq!(
            th(json!({"type": "enabled", "budget_tokens": 2048})).mode,
            ReasoningMode::On {
                budget_tokens: Some(2048)
            }
        );
        assert_eq!(
            th(json!({"type": "adaptive"})).mode,
            ReasoningMode::Adaptive
        );
        assert_eq!(th(json!({"type": "disabled"})).mode, ReasoningMode::Off);
        assert!(parse(with(json!({"thinking": {"type": "psychic"}}))).is_err());
        assert!(parse(with(json!({"thinking": {}}))).is_err());
        assert!(parse(with(json!({"thinking": {"type": "enabled", "mystery": 1}}))).is_err());
    }

    #[test]
    fn prompt_cache_key_sets_the_cache_hint() {
        assert!(ok(with(json!({"prompt_cache_key": "k"}))).value.cache_hints);
        assert!(!ok(base()).value.cache_hints);
        assert!(parse(with(json!({"prompt_cache_key": 3}))).is_err());
    }

    #[test]
    fn wrongly_typed_fields_name_the_field() {
        for (k, v) in [
            ("stream", json!("yes")),
            ("temperature", json!("hot")),
            ("top_k", json!(-1)),
            ("seed", json!("s")),
            ("parallel_tool_calls", json!("no")),
            ("tools", json!("x")),
            ("model", json!(3)),
        ] {
            assert_eq!(parse(with(json!({ k: v }))).unwrap_err().field, k);
        }
    }
}
