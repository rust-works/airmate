//! The OpenAI Responses inbound adapter (`POST /v1/responses`).
//!
//! Every request field, input item, content part, tool and `reasoning` key is looked up in the
//! policy table ([`crate::policy::responses::TABLE`]). A reasoning item a client echoes back
//! carries the gateway's signature in `encrypted_content`; when it verifies, the upstream's own
//! encrypted state is recovered from it so the next turn can replay the reasoning.

use serde_json::{Map, Value as Json};

use super::chat::image_source;
use super::{
    invalid, parse_object, want_array, want_bool, want_f64, want_object, want_str, want_u32,
    InboundCtx,
};
use crate::ir::{
    Effort, IrRequest, Part, Protocol, Reasoning, ReasoningDisplay, ReasoningMode, ResponseFormat,
    SystemPart, ToolChoiceKind, ToolDef, ToolResult, Turn,
};
use crate::policy::{responses::TABLE, Decision, Group, Parsed, Policy, PolicyError};
use crate::signature::verify;

/// Parses a Responses request body.
///
/// # Errors
///
/// A [`PolicyError`] naming the field when the body is malformed, a field is rejected, or a field
/// is unknown and `unknown_fields` is `reject`.
pub fn parse_request(body: &[u8], ctx: &InboundCtx<'_>) -> Result<Parsed<IrRequest>, PolicyError> {
    let obj = parse_object(body)?;
    let mut p = Parser {
        policy: Policy::new(TABLE, ctx.policy),
        ctx: *ctx,
    };
    let ir = p.request(&obj)?;
    Ok(Parsed {
        value: ir,
        ignored: p.policy.into_ignored(),
    })
}

struct Parser<'a> {
    policy: Policy,
    ctx: InboundCtx<'a>,
}

impl Parser<'_> {
    fn request(&mut self, obj: &Map<String, Json>) -> Result<IrRequest, PolicyError> {
        let mut ir = IrRequest::new(String::new(), Protocol::Responses);
        ir.extensions.stream_usage = true;
        let mut have_model = false;
        let mut system: Vec<SystemPart> = Vec::new();
        let mut input: Option<&Json> = None;

        for (key, value) in obj {
            if self.policy.check(Group::Request, key)? == Decision::Skip {
                continue;
            }
            match key.as_str() {
                "model" => {
                    ir.model = want_str(value, "model")?.to_owned();
                    have_model = true;
                }
                "input" => input = Some(value),
                "instructions" => {
                    if let Some(text) = value.as_str().filter(|t| !t.is_empty()) {
                        system.insert(
                            0,
                            SystemPart {
                                text: text.to_owned(),
                                after_turns: 0,
                                effort: None,
                            },
                        );
                    }
                }
                "max_output_tokens" => {
                    let n = want_u32(value, "max_output_tokens")?;
                    if n == 0 {
                        return Err(invalid(
                            "max_output_tokens",
                            "`max_output_tokens` must be greater than 0",
                        ));
                    }
                    ir.max_output_tokens = n;
                }
                "temperature" => ir.sampling.temperature = Some(want_f64(value, "temperature")?),
                "top_p" => ir.sampling.top_p = Some(want_f64(value, "top_p")?),
                "stream" => ir.stream = want_bool(value, "stream")?,
                "tools" => ir.tools = self.tools(want_array(value, "tools")?)?,
                "tool_choice" => ir.tool_choice.kind = self.tool_choice(value)?,
                "parallel_tool_calls" => {
                    ir.tool_choice.parallel = want_bool(value, "parallel_tool_calls")?
                }
                "reasoning" => ir.reasoning = self.reasoning(value)?,
                "text" => ir.response_format = self.text(value)?,
                "include" => {
                    for (i, v) in want_array(value, "include")?.iter().enumerate() {
                        let name = want_str(v, &format!("include[{i}]"))?;
                        if name != "reasoning.encrypted_content" {
                            self.policy
                                .ignore(&format!("include.{name}"), "no upstream equivalent");
                        }
                    }
                }
                "prompt_cache_key" => {
                    want_str(value, "prompt_cache_key")?;
                    ir.cache_hints = true;
                }
                other => {
                    unreachable!("table says `{other}` is translated but the parser has no case")
                }
            }
        }
        if !have_model {
            return Err(invalid("model", "`model` is required"));
        }
        let input = input.ok_or_else(|| invalid("input", "`input` is required"))?;
        self.input(input, &mut ir, &mut system)?;
        ir.system = system;
        Ok(ir)
    }

    fn reasoning(&mut self, value: &Json) -> Result<Reasoning, PolicyError> {
        let obj = want_object(value, "reasoning")?;
        let mut effort = None;
        let mut off = false;
        let mut wants_summary = false;
        for (k, v) in obj {
            if self.policy.check(Group::ThinkingField, k)? == Decision::Skip {
                continue;
            }
            match k.as_str() {
                "effort" => match v.as_str() {
                    Some("none") => off = true,
                    Some("minimal") => effort = Some(Effort::Low),
                    Some(e) => {
                        effort = Some(Effort::parse(e).ok_or_else(|| {
                            invalid("reasoning.effort", format!("unknown effort `{e}`; expected none, minimal, low, medium, high, xhigh or max"))
                        })?);
                    }
                    None if v.is_null() => {}
                    None => return Err(invalid("reasoning.effort", "`effort` must be a string")),
                },
                "summary" | "generate_summary" => wants_summary |= !v.is_null(),
                other => unreachable!("table says reasoning key `{other}` is translated"),
            }
        }
        let mode = if off {
            ReasoningMode::Off
        } else if wants_summary || effort.is_some() {
            ReasoningMode::Adaptive
        } else {
            ReasoningMode::Unspecified
        };
        Ok(Reasoning {
            mode,
            effort,
            display: ReasoningDisplay::Summarized,
        })
    }

    fn text(&mut self, value: &Json) -> Result<ResponseFormat, PolicyError> {
        let obj = want_object(value, "text")?;
        for k in obj.keys() {
            match k.as_str() {
                "format" => {}
                "verbosity" => self
                    .policy
                    .ignore("text.verbosity", "no upstream equivalent"),
                other => {
                    return Err(invalid(
                        format!("text.{other}"),
                        format!("`text.{other}` is not known to airmate"),
                    ));
                }
            }
        }
        let Some(f) = obj.get("format") else {
            return Ok(ResponseFormat::Text);
        };
        let f = want_object(f, "text.format")?;
        match f.get("type").and_then(Json::as_str) {
            Some("text") => Ok(ResponseFormat::Text),
            Some("json_object") => Ok(ResponseFormat::JsonObject),
            Some("json_schema") => {
                let schema = f
                    .get("schema")
                    .ok_or_else(|| invalid("text.format.schema", "`schema` is required"))?;
                Ok(ResponseFormat::JsonSchema {
                    schema: schema.clone(),
                    strict: f.get("strict").and_then(Json::as_bool).unwrap_or(false),
                })
            }
            _ => Err(invalid(
                "text.format.type",
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
            let mut name = None;
            let mut description = None;
            let mut schema = None;
            let mut strict = None;
            for (k, v) in obj {
                let kp = format!("{path}.{k}");
                if k == "type" {
                    let ty = want_str(v, &kp)?;
                    if ty != "function" {
                        return Err(invalid(kp, format!("tool type `{ty}` is not supported by airmate; only `function` tools are")));
                    }
                    continue;
                }
                if self.policy.check(Group::ToolField, k)? == Decision::Skip {
                    continue;
                }
                match k.as_str() {
                    "name" => name = Some(want_str(v, &kp)?.to_owned()),
                    "description" => description = v.as_str().map(str::to_owned),
                    "parameters" => {
                        if !v.is_null() {
                            want_object(v, &kp)?;
                            schema = Some(v.clone());
                        }
                    }
                    "strict" => strict = v.as_bool(),
                    other => unreachable!("table says tool field `{other}` is translated"),
                }
            }
            out.push(ToolDef {
                name: name.ok_or_else(|| invalid(format!("{path}.name"), "`name` is required"))?,
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
                        "name" => name = Some(want_str(v, "tool_choice.name")?.to_owned()),
                        other => {
                            unreachable!("table says tool_choice field `{other}` is translated")
                        }
                    }
                }
                name.map(ToolChoiceKind::Named)
                    .ok_or_else(|| invalid("tool_choice.name", "`name` is required"))
            }
            _ => Err(invalid(
                "tool_choice",
                "`tool_choice` must be a string or an object",
            )),
        }
    }

    // ---- input ------------------------------------------------------------------------------

    fn input(
        &mut self,
        input: &Json,
        ir: &mut IrRequest,
        system: &mut Vec<SystemPart>,
    ) -> Result<(), PolicyError> {
        match input {
            Json::String(s) => {
                ir.turns.push(Turn::User(vec![Part::Text(s.clone())]));
                Ok(())
            }
            Json::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    self.item(item, &format!("input[{i}]"), ir, system)?;
                }
                Ok(())
            }
            _ => Err(invalid(
                "input",
                "`input` must be a string or an array of items",
            )),
        }
    }

    /// Checks an item's keys against the table and returns the translated ones.
    fn item_keys<'o>(
        &mut self,
        ty: &str,
        obj: &'o Map<String, Json>,
        path: &str,
    ) -> Result<Vec<(&'o str, &'o Json)>, PolicyError> {
        let mut keep = Vec::new();
        for (k, v) in obj {
            if self
                .policy
                .check(Group::BlockField, &format!("{ty}.{k}"))
                .map_err(|mut e| {
                    e.field = format!("{path}.{k}");
                    e
                })?
                == Decision::Translate
            {
                keep.push((k.as_str(), v));
            }
        }
        Ok(keep)
    }

    fn item(
        &mut self,
        item: &Json,
        path: &str,
        ir: &mut IrRequest,
        system: &mut Vec<SystemPart>,
    ) -> Result<(), PolicyError> {
        let obj = want_object(item, path)?;
        // `{role, content}` without a type is a message.
        let ty = obj.get("type").and_then(Json::as_str).unwrap_or("message");
        if self.policy.check(Group::InputItem, ty)? == Decision::Skip {
            return Ok(());
        }
        let keys = self.item_keys(ty, obj, path)?;
        let get = |name: &str| keys.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
        let need = |name: &str| {
            get(name)
                .ok_or_else(|| invalid(format!("{path}.{name}"), format!("`{name}` is required")))
        };
        match ty {
            "message" => {
                let role = want_str(need("role")?, &format!("{path}.role"))?;
                let parts = self.content(get("content"), path)?;
                match role {
                    "user" => ir.turns.push(Turn::User(parts)),
                    // Reasoning, text and function calls that follow one another are one turn.
                    "assistant" => match ir.turns.last_mut() {
                        Some(Turn::Assistant(existing)) => existing.extend(parts),
                        _ => ir.turns.push(Turn::Assistant(parts)),
                    },
                    "system" | "developer" => {
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
                    other => {
                        return Err(invalid(format!("{path}.role"), format!("unknown role `{other}`; expected user, assistant, system or developer")));
                    }
                }
            }
            "function_call" => {
                let call_id = want_str(need("call_id")?, &format!("{path}.call_id"))?.to_owned();
                let name = want_str(need("name")?, &format!("{path}.name"))?.to_owned();
                let args = get("arguments").and_then(Json::as_str).unwrap_or("");
                let input = if args.trim().is_empty() {
                    serde_json::json!({})
                } else {
                    serde_json::from_str(args).map_err(|e| {
                        invalid(
                            format!("{path}.arguments"),
                            format!("the arguments of `{name}` are not valid JSON: {e}"),
                        )
                    })?
                };
                let call = Part::ToolCall {
                    id: call_id,
                    name,
                    input,
                };
                match ir.turns.last_mut() {
                    Some(Turn::Assistant(parts)) => parts.push(call),
                    _ => ir.turns.push(Turn::Assistant(vec![call])),
                }
            }
            "function_call_output" => {
                let call_id = want_str(need("call_id")?, &format!("{path}.call_id"))?.to_owned();
                let content = match get("output") {
                    Some(Json::String(s)) => vec![Part::Text(s.clone())],
                    Some(Json::Array(_)) => self.content(get("output"), path)?,
                    None | Some(Json::Null) => Vec::new(),
                    Some(other) => vec![Part::Text(other.to_string())],
                };
                let result = ToolResult {
                    call_id,
                    content,
                    is_error: false,
                };
                match ir.turns.last_mut() {
                    Some(Turn::ToolResults(rs)) => rs.push(result),
                    _ => ir.turns.push(Turn::ToolResults(vec![result])),
                }
            }
            "reasoning" => {
                let text = match get("summary") {
                    Some(Json::Array(parts)) => parts
                        .iter()
                        .filter_map(|p| p.get("text").and_then(Json::as_str))
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                    _ => String::new(),
                };
                let verified = get("encrypted_content")
                    .and_then(Json::as_str)
                    .and_then(|sig| {
                        self.ctx
                            .signing_key
                            .and_then(|k| verify(k, sig, &text).ok())
                    });
                match verified {
                    Some(payload) => {
                        let part = Part::Reasoning {
                            text,
                            display: ReasoningDisplay::Summarized,
                            origin: Some(payload.origin),
                            opaque: payload.encrypted_content,
                        };
                        match ir.turns.last_mut() {
                            Some(Turn::Assistant(parts)) => parts.push(part),
                            _ => ir.turns.push(Turn::Assistant(vec![part])),
                        }
                    }
                    None => self.policy.ignore(
                        "thinking_unverified",
                        "the reasoning item was not signed by this gateway, or its token changed",
                    ),
                }
            }
            other => unreachable!("table says input item `{other}` is translated"),
        }
        Ok(())
    }

    fn content(&mut self, content: Option<&Json>, path: &str) -> Result<Vec<Part>, PolicyError> {
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
                    if self.policy.check(Group::ContentPart, ty)? == Decision::Skip {
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
                    let field = |name: &str| -> Result<&str, PolicyError> {
                        want_str(
                            obj.get(name).ok_or_else(|| {
                                invalid(format!("{pp}.{name}"), format!("`{name}` is required"))
                            })?,
                            &format!("{pp}.{name}"),
                        )
                    };
                    match ty {
                        "input_text" | "output_text" => {
                            out.push(Part::Text(field("text")?.to_owned()))
                        }
                        "refusal" => out.push(Part::Text(field("refusal")?.to_owned())),
                        "input_image" => out.push(Part::Image(image_source(field("image_url")?))),
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
    use crate::ir::{ImageSource, Origin};
    use crate::policy::{PolicyConfig, UnknownFields};
    use crate::signature::{sign, SignaturePayload, SigningKey};
    use serde_json::json;

    fn parse(v: Json) -> Result<Parsed<IrRequest>, PolicyError> {
        parse_request(v.to_string().as_bytes(), &InboundCtx::default())
    }

    fn ok(v: Json) -> Parsed<IrRequest> {
        parse(v).expect("parses")
    }

    fn with(extra: Json) -> Json {
        let mut b = json!({"model": "gpt-5", "input": "hi"});
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    }

    #[test]
    fn a_string_input_is_one_user_turn() {
        let p = ok(with(json!({})));
        assert_eq!(
            p.value.turns,
            vec![Turn::User(vec![Part::Text("hi".into())])]
        );
        assert_eq!(p.value.extensions.inbound, Protocol::Responses);
        assert_eq!(p.value.max_output_tokens, 0, "unset");
        assert!(p.value.extensions.stream_usage && p.ignored.is_empty());
    }

    #[test]
    fn model_and_input_are_required() {
        for key in ["model", "input"] {
            let mut b = with(json!({}));
            b.as_object_mut().unwrap().remove(key);
            assert_eq!(parse(b).unwrap_err().field, key);
        }
        assert!(parse(with(json!({"input": 5}))).is_err());
        assert!(parse_request(b"[]", &InboundCtx::default()).is_err());
    }

    #[test]
    fn instructions_lead_and_scalar_fields_parse() {
        let p = ok(with(
            json!({"instructions": "be brief", "max_output_tokens": 50, "temperature": 0.5, "top_p": 0.8, "stream": true, "parallel_tool_calls": false, "prompt_cache_key": "k"}),
        ));
        let ir = p.value;
        assert_eq!(
            (ir.system[0].text.as_str(), ir.system[0].after_turns),
            ("be brief", 0)
        );
        assert_eq!(
            (
                ir.max_output_tokens,
                ir.sampling.temperature,
                ir.sampling.top_p,
                ir.stream
            ),
            (50, Some(0.5), Some(0.8), true)
        );
        assert!(!ir.tool_choice.parallel && ir.cache_hints);
        assert!(parse(with(json!({"max_output_tokens": 0}))).is_err());
        assert!(parse(with(json!({"stream": "yes"}))).is_err());
    }

    #[test]
    fn server_side_state_is_rejected_by_name() {
        for field in [
            "previous_response_id",
            "conversation",
            "background",
            "prompt",
            "top_logprobs",
        ] {
            let err = parse(with(json!({ field: "x" }))).unwrap_err();
            assert_eq!(err.field, field);
        }
    }

    #[test]
    fn harmless_fields_are_ignored_and_reported() {
        let p = ok(with(
            json!({"store": true, "metadata": {}, "user": "u", "service_tier": "auto", "truncation": "auto", "max_tool_calls": 3, "safety_identifier": "s", "stream_options": {}}),
        ));
        let names: Vec<_> = p.ignored.iter().map(|i| i.name.as_str()).collect();
        for n in [
            "store",
            "metadata",
            "user",
            "service_tier",
            "truncation",
            "max_tool_calls",
            "safety_identifier",
            "stream_options",
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
        assert_eq!(
            parse_request(with(json!({"shiny": 1})).to_string().as_bytes(), &ctx)
                .unwrap()
                .ignored[0]
                .name,
            "shiny"
        );
    }

    #[test]
    fn message_items_with_roles_and_content_parts() {
        let p = ok(json!({"model": "m", "input": [
            {"role": "developer", "content": "rules"},
            {"type": "message", "role": "user", "id": "msg_1", "status": "completed", "content": [
                {"type": "input_text", "text": "look"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA", "detail": "high"},
                {"type": "input_image", "image_url": "https://x/y.png"}]},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "I see", "annotations": []}, {"type": "refusal", "refusal": "but no"}]},
            {"role": "user", "content": "next"}]}));
        assert_eq!(p.value.system[0].text, "rules");
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
        assert_eq!(
            p.value.turns[1],
            Turn::Assistant(vec![
                Part::Text("I see".into()),
                Part::Text("but no".into())
            ])
        );
        let names: Vec<_> = p.ignored.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"input_image.detail") && names.contains(&"output_text.annotations")
        );
        assert!(
            !names.contains(&"message.id"),
            "echoed ids are accepted quietly"
        );
    }

    #[test]
    fn malformed_items_and_parts_are_rejected() {
        let one = |item: Json| parse(json!({"model": "m", "input": [item]}));
        assert!(one(json!({"role": "robot", "content": "x"}))
            .unwrap_err()
            .message
            .contains("unknown role"));
        assert!(one(json!({"content": "x"})).is_err());
        assert!(one(json!("x")).is_err());
        assert!(
            one(json!({"role": "user", "content": [{"type": "input_file", "file_id": "f"}]}))
                .is_err()
        );
        assert!(one(json!({"role": "user", "content": [{"type": "input_audio"}]})).is_err());
        assert!(
            one(json!({"role": "user", "content": [{"type": "input_image", "file_id": "f"}]}))
                .unwrap_err()
                .message
                .contains("file ids")
        );
        assert!(one(json!({"role": "user", "content": [{"type": "hologram"}]})).is_err());
        assert!(one(json!({"role": "user", "content": [{"text": "x"}]})).is_err());
        assert!(one(json!({"role": "user", "content": 4})).is_err());
        assert!(one(json!({"role": "user", "content": "x", "bogus": 1})).is_err());
        for ty in [
            "item_reference",
            "web_search_call",
            "computer_call",
            "mcp_call",
            "code_interpreter_call",
        ] {
            assert_eq!(one(json!({"type": ty, "id": "x"})).unwrap_err().field, ty);
        }
    }

    #[test]
    fn function_calls_and_outputs_group_into_turns() {
        let p = ok(json!({"model": "m", "input": [
            {"role": "user", "content": "go"},
            {"type": "function_call", "call_id": "c1", "name": "read", "arguments": "{\"p\":1}"},
            {"type": "function_call", "call_id": "c2", "name": "ls", "arguments": ""},
            {"type": "function_call_output", "call_id": "c1", "output": "contents"},
            {"type": "function_call_output", "call_id": "c2", "output": [{"type": "input_text", "text": "listing"}]},
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
                assert_eq!((rs.len(), rs[0].call_id.as_str()), (2, "c1"));
                assert_eq!(rs[1].content, vec![Part::Text("listing".into())]);
            }
            other => panic!("{other:?}"),
        }
        let bad = parse(
            json!({"model": "m", "input": [{"type": "function_call", "call_id": "c", "name": "n", "arguments": "{bad"}]}),
        );
        assert!(bad.unwrap_err().message.contains("not valid JSON"));
        assert!(
            parse(json!({"model": "m", "input": [{"type": "function_call", "name": "n"}]}))
                .is_err()
        );
    }

    #[test]
    fn tools_choice_and_response_format() {
        let p = ok(with(
            json!({"tools": [{"type": "function", "name": "read", "description": "d", "parameters": {"type": "object"}, "strict": true}, {"type": "function", "name": "bare", "parameters": null}],
            "tool_choice": {"type": "function", "name": "read"}}),
        ));
        assert_eq!(p.value.tools[0].strict, Some(true));
        assert_eq!(
            p.value.tools[1].input_schema,
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(
            p.value.tool_choice.kind,
            ToolChoiceKind::Named("read".into())
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
        for web in [
            "web_search_preview",
            "file_search",
            "computer_use_preview",
            "mcp",
            "code_interpreter",
            "image_generation",
            "custom",
        ] {
            assert!(
                parse(with(json!({"tools": [{"type": web}]})))
                    .unwrap_err()
                    .message
                    .contains("only `function`"),
                "{web}"
            );
        }
        assert!(parse(with(json!({"tool_choice": {"type": "allowed_tools"}}))).is_err());
        assert!(parse(with(json!({"tool_choice": "maybe"}))).is_err());
        assert!(parse(with(
            json!({"tools": [{"type": "function", "name": "n", "weird": 1}]})
        ))
        .is_err());
        assert!(parse(with(json!({"tools": [{"type": "function"}]}))).is_err());
        let fmt = |v: Json| ok(with(json!({"text": v}))).value.response_format;
        assert_eq!(
            fmt(json!({"format": {"type": "text"}})),
            ResponseFormat::Text
        );
        assert_eq!(
            fmt(json!({"format": {"type": "json_object"}})),
            ResponseFormat::JsonObject
        );
        assert_eq!(
            fmt(
                json!({"format": {"type": "json_schema", "name": "x", "schema": {"type": "object"}, "strict": true}})
            ),
            ResponseFormat::JsonSchema {
                schema: json!({"type": "object"}),
                strict: true
            }
        );
        assert_eq!(fmt(json!({})), ResponseFormat::Text);
        assert!(ok(with(json!({"text": {"verbosity": "low"}})))
            .ignored
            .iter()
            .any(|i| i.name == "text.verbosity"));
        assert!(parse(with(json!({"text": {"format": {"type": "xml"}}}))).is_err());
        assert!(parse(with(json!({"text": {"mystery": 1}}))).is_err());
        assert!(parse(with(json!({"text": {"format": {"type": "json_schema"}}}))).is_err());
    }

    #[test]
    fn reasoning_controls() {
        let r = |v: Json| ok(with(json!({"reasoning": v}))).value.reasoning;
        assert_eq!(r(json!({"effort": "high"})).effort, Some(Effort::High));
        assert_eq!(r(json!({"effort": "high"})).mode, ReasoningMode::Adaptive);
        assert_eq!(r(json!({"effort": "minimal"})).effort, Some(Effort::Low));
        assert_eq!(r(json!({"effort": "none"})).mode, ReasoningMode::Off);
        assert_eq!(
            r(json!({"summary": "auto"})).mode,
            ReasoningMode::Adaptive,
            "asking for a summary asks for reasoning"
        );
        assert_eq!(r(json!({"summary": null})).mode, ReasoningMode::Unspecified);
        assert_eq!(r(json!({"effort": null})).effort, None);
        assert_eq!(
            ok(with(json!({}))).value.reasoning.mode,
            ReasoningMode::Unspecified
        );
        assert!(parse(with(json!({"reasoning": {"effort": "extreme"}}))).is_err());
        assert!(parse(with(json!({"reasoning": {"effort": 5}}))).is_err());
        assert!(parse(with(json!({"reasoning": {"mystery": 1}}))).is_err());
    }

    #[test]
    fn include_is_accepted_and_other_values_are_ignored() {
        let p = ok(with(
            json!({"include": ["reasoning.encrypted_content", "message.output_text.logprobs"]}),
        ));
        let names: Vec<_> = p.ignored.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["include.message.output_text.logprobs"]);
        assert!(parse(with(json!({"include": "x"}))).is_err());
    }

    fn reasoning_history(key: &SigningKey, summary: &str, payload: &SignaturePayload) -> Json {
        json!({"model": "m", "input": [
            {"role": "user", "content": "q"},
            {"type": "reasoning", "id": "rs_1", "summary": [{"type": "summary_text", "text": summary}], "encrypted_content": sign(key, payload, summary)},
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "a"}]},
            {"role": "user", "content": "next"}]})
    }

    #[test]
    fn a_signed_reasoning_item_is_recovered_with_the_upstreams_encrypted_state() {
        let key = SigningKey::from_token("tok");
        let payload = SignaturePayload {
            origin: Origin {
                upstream: "oa".into(),
                protocol: Protocol::Responses,
            },
            encrypted_content: Some(b"UPSTREAM-ENC".to_vec()),
            hidden_text: None,
        };
        let body = reasoning_history(&key, "why", &payload);
        let ctx = InboundCtx {
            signing_key: Some(&key),
            ..InboundCtx::default()
        };
        let p = parse_request(body.to_string().as_bytes(), &ctx).unwrap();
        match &p.value.turns[1] {
            Turn::Assistant(parts) => {
                assert!(
                    matches!(&parts[0], Part::Reasoning { text, origin: Some(o), opaque: Some(e), .. } if text == "why" && o.upstream == "oa" && e == b"UPSTREAM-ENC")
                );
                assert_eq!(parts[1], Part::Text("a".into()));
            }
            other => panic!("{other:?}"),
        }
        assert!(p.ignored.is_empty());
    }

    #[test]
    fn an_unsigned_or_foreign_reasoning_item_is_dropped_and_reported() {
        let key = SigningKey::from_token("tok");
        let payload = SignaturePayload {
            origin: Origin {
                upstream: "oa".into(),
                protocol: Protocol::Responses,
            },
            encrypted_content: None,
            hidden_text: None,
        };
        let body = reasoning_history(&key, "why", &payload);
        let other = SigningKey::from_token("other");
        for ctx in [
            InboundCtx {
                signing_key: Some(&other),
                ..InboundCtx::default()
            },
            InboundCtx::default(),
        ] {
            let p = parse_request(body.to_string().as_bytes(), &ctx).unwrap();
            assert!(p.ignored.iter().any(|i| i.name == "thinking_unverified"));
            assert!(
                matches!(&p.value.turns[1], Turn::Assistant(parts) if parts.len() == 1 && matches!(parts[0], Part::Text(_)))
            );
        }
        let foreign = json!({"model": "m", "input": [{"type": "reasoning", "summary": [], "encrypted_content": "gAAAAB-openai-blob"}, {"role": "user", "content": "q"}]});
        let ctx = InboundCtx {
            signing_key: Some(&key),
            ..InboundCtx::default()
        };
        assert!(parse_request(foreign.to_string().as_bytes(), &ctx)
            .unwrap()
            .ignored
            .iter()
            .any(|i| i.name == "thinking_unverified"));
    }
}
