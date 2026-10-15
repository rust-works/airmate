//! The Anthropic Messages inbound adapter (`POST /v1/messages`).
//!
//! Every request field, message key, content block, block field, tool field, `thinking` and
//! `output_config` key is looked up in the policy table ([`crate::policy::anthropic::TABLE`]); the
//! parser only handles what the table says is translated.

use serde_json::{Map, Value as Json};

use super::{
    invalid, parse_object, unhandled, want_array, want_bool, want_f64, want_object, want_str,
    want_u32, InboundCtx,
};
use crate::ir::{
    DocumentSource, Effort, ImageSource, IrRequest, Part, Protocol, Reasoning, ReasoningDisplay,
    ReasoningMode, ResponseFormat, SystemPart, ToolChoice, ToolChoiceKind, ToolDef, ToolResult,
    Turn,
};
use crate::policy::{anthropic::TABLE, Decision, Group, Parsed, Policy, PolicyError};
use crate::signature::verify;

/// Parses an Anthropic Messages request body.
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
        cache_hints: false,
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
    cache_hints: bool,
}

impl Parser<'_> {
    fn request(&mut self, obj: &Map<String, Json>) -> Result<IrRequest, PolicyError> {
        let mut ir = IrRequest::new(String::new(), Protocol::Anthropic);
        let mut have_model = false;
        let mut have_max = false;
        let mut have_messages = false;
        let mut system: Vec<SystemPart> = Vec::new();
        let mut effort_from_config: Option<Effort> = None;

        for (key, value) in obj {
            if self.policy.check(Group::Request, key)? == Decision::Skip {
                continue;
            }
            match key.as_str() {
                "model" => {
                    ir.model = want_str(value, "model")?.to_owned();
                    have_model = true;
                }
                "max_tokens" => {
                    let n = want_u32(value, "max_tokens")?;
                    if n == 0 {
                        return Err(invalid("max_tokens", "`max_tokens` must be greater than 0"));
                    }
                    ir.max_output_tokens = n;
                    have_max = true;
                }
                "messages" => {
                    self.messages(want_array(value, "messages")?, &mut ir, &mut system)?;
                    have_messages = true;
                }
                "system" => {
                    let parts = self.system_prompt(value)?;
                    system.splice(0..0, parts);
                }
                "stop_sequences" => {
                    for (i, s) in want_array(value, "stop_sequences")?.iter().enumerate() {
                        ir.stop
                            .push(want_str(s, &format!("stop_sequences[{i}]"))?.to_owned());
                    }
                }
                "stream" => ir.stream = want_bool(value, "stream")?,
                "temperature" => ir.sampling.temperature = Some(want_f64(value, "temperature")?),
                "top_p" => ir.sampling.top_p = Some(want_f64(value, "top_p")?),
                "top_k" => ir.sampling.top_k = Some(want_u32(value, "top_k")?),
                "tools" => ir.tools = self.tools(want_array(value, "tools")?)?,
                "tool_choice" => ir.tool_choice = self.tool_choice(value)?,
                "thinking" => ir.reasoning = self.thinking(value)?,
                "output_config" => {
                    let (effort, format) = self.output_config(value, "output_config")?;
                    effort_from_config = effort;
                    if let Some(f) = format {
                        ir.response_format = f;
                    }
                }
                "cache_control" => {
                    self.cache_control(value, "cache_control")?;
                }
                other => {
                    unhandled!("table says `{other}` is translated but the parser has no case")
                }
            }
        }

        if !have_model {
            return Err(invalid("model", "`model` is required"));
        }
        if !have_max && !self.ctx.count_tokens {
            return Err(invalid("max_tokens", "`max_tokens` is required"));
        }
        if !have_messages {
            return Err(invalid("messages", "`messages` is required"));
        }
        if effort_from_config.is_some() {
            ir.reasoning.effort = effort_from_config;
        }
        ir.system = system;
        ir.cache_hints = self.cache_hints;
        relocate_tool_results(&mut ir.turns);
        Ok(ir)
    }

    // ---- system -----------------------------------------------------------------------------

    fn system_prompt(&mut self, value: &Json) -> Result<Vec<SystemPart>, PolicyError> {
        let mk = |text: String| SystemPart {
            text,
            after_turns: 0,
            effort: None,
        };
        match value {
            Json::String(s) => Ok(vec![mk(s.clone())]),
            Json::Array(blocks) => {
                let mut out = Vec::new();
                for (i, b) in blocks.iter().enumerate() {
                    let path = format!("system[{i}]");
                    let parts = self.block(b, &path)?;
                    for part in parts {
                        match part {
                            Part::Text(t) => out.push(mk(t)),
                            _ => {
                                return Err(invalid(&path, "system blocks must be text blocks"));
                            }
                        }
                    }
                }
                Ok(out)
            }
            _ => Err(invalid(
                "system",
                "`system` must be a string or an array of text blocks",
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
        let last = messages.len().saturating_sub(1);
        for (i, m) in messages.iter().enumerate() {
            let path = format!("messages[{i}]");
            let obj = want_object(m, &path)?;
            let mut role: Option<&str> = None;
            let mut content: Option<&Json> = None;
            let mut effort: Option<Effort> = None;
            for (k, v) in obj {
                if self.policy.check(Group::MessageKey, k)? == Decision::Skip {
                    continue;
                }
                match k.as_str() {
                    "role" => role = Some(want_str(v, &format!("{path}.role"))?),
                    "content" => content = Some(v),
                    "output_config" => {
                        effort = self.output_config(v, &format!("{path}.output_config"))?.0;
                    }
                    other => unhandled!("table says message key `{other}` is translated"),
                }
            }
            let role = role.ok_or_else(|| invalid(format!("{path}.role"), "`role` is required"))?;
            let content = content
                .ok_or_else(|| invalid(format!("{path}.content"), "`content` is required"))?;
            let blocks = self.content(content, &format!("{path}.content"))?;

            match role {
                "user" => self.user_turn(blocks, ir),
                "assistant" => {
                    ir.turns.push(Turn::Assistant(blocks.parts));
                    if i == last {
                        self.policy.ignore(
                            "assistant_prefill",
                            "a trailing assistant message is sent as a finished turn",
                        );
                    }
                }
                "system" => {
                    for part in blocks.parts {
                        if let Part::Text(text) = part {
                            system.push(SystemPart {
                                text,
                                after_turns: ir.turns.len(),
                                effort,
                            });
                        } else {
                            return Err(invalid(&path, "system messages may contain only text"));
                        }
                    }
                }
                other => {
                    return Err(invalid(
                        format!("{path}.role"),
                        format!("unknown role `{other}`; expected `user`, `assistant` or `system`"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// A user message may mix tool results and other content. Tool results come first.
    fn user_turn(&mut self, blocks: Blocks, ir: &mut IrRequest) {
        if !blocks.tool_results.is_empty() {
            ir.turns.push(Turn::ToolResults(blocks.tool_results));
        }
        if !blocks.parts.is_empty() {
            ir.turns.push(Turn::User(blocks.parts));
        }
    }

    fn content(&mut self, content: &Json, path: &str) -> Result<Blocks, PolicyError> {
        let mut out = Blocks::default();
        match content {
            Json::String(s) => out.parts.push(Part::Text(s.clone())),
            Json::Array(blocks) => {
                for (i, b) in blocks.iter().enumerate() {
                    let bp = format!("{path}[{i}]");
                    let obj = want_object(b, &bp)?;
                    if obj.get("type").and_then(Json::as_str) == Some("tool_result") {
                        out.tool_results.push(self.tool_result(obj, &bp)?);
                    } else {
                        out.parts.extend(self.block(b, &bp)?);
                    }
                }
            }
            _ => {
                return Err(invalid(
                    path,
                    format!("`{path}` must be a string or an array of blocks"),
                ));
            }
        }
        Ok(out)
    }

    // ---- blocks -----------------------------------------------------------------------------

    /// Checks every key of a block against the table, returning the translated keys.
    fn block_keys<'o>(
        &mut self,
        ty: &str,
        obj: &'o Map<String, Json>,
        path: &str,
    ) -> Result<Vec<(&'o str, &'o Json)>, PolicyError> {
        let mut keep = Vec::new();
        for (k, v) in obj {
            if k == "type" {
                continue;
            }
            let name = format!("{ty}.{k}");
            if self
                .policy
                .check(Group::BlockField, &name)
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

    fn block(&mut self, b: &Json, path: &str) -> Result<Vec<Part>, PolicyError> {
        let obj = want_object(b, path)?;
        let ty = obj
            .get("type")
            .ok_or_else(|| invalid(format!("{path}.type"), "a content block needs a `type`"))
            .and_then(|t| want_str(t, &format!("{path}.type")))?;
        if self.policy.check(Group::ContentBlock, ty)? == Decision::Skip {
            return Ok(Vec::new());
        }
        let keys = self.block_keys(ty, obj, path)?;
        let get = |name: &str| keys.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
        let need = |name: &str| {
            get(name)
                .ok_or_else(|| invalid(format!("{path}.{name}"), format!("`{name}` is required")))
        };
        if let Some(cc) = get("cache_control") {
            self.cache_control(cc, &format!("{path}.cache_control"))?;
        }
        match ty {
            "text" => Ok(vec![Part::Text(
                want_str(need("text")?, &format!("{path}.text"))?.to_owned(),
            )]),
            "image" => Ok(vec![Part::Image(
                self.image_source(need("source")?, &format!("{path}.source"))?,
            )]),
            "document" => self.document(need("source")?, &format!("{path}.source")),
            "compaction" => {
                // A block that carries `null` marks a failed compaction and is a no-op.
                match get("content") {
                    Some(Json::String(s)) if !s.is_empty() => Ok(vec![Part::Text(s.clone())]),
                    Some(Json::String(_)) => Err(invalid(
                        format!("{path}.content"),
                        "a compaction block's `content` may not be an empty string",
                    )),
                    _ => Ok(Vec::new()),
                }
            }
            "search_result" => {
                let source = want_str(need("source")?, &format!("{path}.source"))?;
                let title = want_str(need("title")?, &format!("{path}.title"))?;
                let mut text = format!("Source: {source}\nTitle: {title}");
                for (i, c) in want_array(need("content")?, &format!("{path}.content"))?
                    .iter()
                    .enumerate()
                {
                    let cp = format!("{path}.content[{i}]");
                    let co = want_object(c, &cp)?;
                    let t = co
                        .get("text")
                        .ok_or_else(|| invalid(&cp, "search result content must be text blocks"))?;
                    text.push('\n');
                    text.push_str(want_str(t, &format!("{cp}.text"))?);
                }
                Ok(vec![Part::Text(text)])
            }
            "thinking" => {
                let thinking = want_str(need("thinking")?, &format!("{path}.thinking"))?;
                let signature = want_str(need("signature")?, &format!("{path}.signature"))?;
                Ok(self.thinking_block(thinking, signature))
            }
            "tool_use" => {
                let id = want_str(need("id")?, &format!("{path}.id"))?.to_owned();
                let name = want_str(need("name")?, &format!("{path}.name"))?.to_owned();
                let input = need("input")?.clone();
                Ok(vec![Part::ToolCall { id, name, input }])
            }
            "tool_reference" => {
                let name = want_str(need("tool_name")?, &format!("{path}.tool_name"))?;
                Ok(vec![Part::Text(format!("Tool: {name}"))])
            }
            other => {
                unhandled!("table says block `{other}` is translated but the parser has no case")
            }
        }
    }

    fn thinking_block(&mut self, thinking: &str, signature: &str) -> Vec<Part> {
        let verified = self
            .ctx
            .signing_key
            .and_then(|k| verify(k, signature, thinking).ok());
        match verified {
            Some(payload) => {
                let hidden = payload.hidden_text.is_some() && thinking.is_empty();
                vec![Part::Reasoning {
                    text: payload.hidden_text.unwrap_or_else(|| thinking.to_owned()),
                    display: if hidden {
                        ReasoningDisplay::Omitted
                    } else {
                        ReasoningDisplay::Summarized
                    },
                    origin: Some(payload.origin),
                    opaque: payload.encrypted_content,
                }]
            }
            None => {
                self.policy.ignore(
                    "thinking_unverified",
                    "the thinking signature was not produced by this gateway, or its token changed",
                );
                Vec::new()
            }
        }
    }

    fn tool_result(
        &mut self,
        obj: &Map<String, Json>,
        path: &str,
    ) -> Result<ToolResult, PolicyError> {
        if self.policy.check(Group::ContentBlock, "tool_result")? == Decision::Skip {
            unreachable!("tool_result is translated");
        }
        let keys = self.block_keys("tool_result", obj, path)?;
        let get = |name: &str| keys.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
        let call_id = want_str(
            get("tool_use_id").ok_or_else(|| {
                invalid(format!("{path}.tool_use_id"), "`tool_use_id` is required")
            })?,
            &format!("{path}.tool_use_id"),
        )?
        .to_owned();
        let is_error = match get("is_error") {
            Some(v) => want_bool(v, &format!("{path}.is_error"))?,
            None => false,
        };
        if let Some(cc) = get("cache_control") {
            self.cache_control(cc, &format!("{path}.cache_control"))?;
        }
        let content = match get("content") {
            None => Vec::new(),
            Some(Json::String(s)) => vec![Part::Text(s.clone())],
            Some(Json::Array(blocks)) => {
                let mut parts = Vec::new();
                for (i, b) in blocks.iter().enumerate() {
                    parts.extend(self.block(b, &format!("{path}.content[{i}]"))?);
                }
                parts
            }
            Some(_) => {
                return Err(invalid(
                    format!("{path}.content"),
                    "`content` must be a string or an array of blocks",
                ));
            }
        };
        Ok(ToolResult {
            call_id,
            content,
            is_error,
        })
    }

    fn image_source(&mut self, source: &Json, path: &str) -> Result<ImageSource, PolicyError> {
        let obj = want_object(source, path)?;
        let ty = want_str(
            obj.get("type")
                .ok_or_else(|| invalid(format!("{path}.type"), "`type` is required"))?,
            &format!("{path}.type"),
        )?;
        self.policy
            .check(Group::BlockField, &format!("image.source.type={ty}"))
            .map_err(|mut e| {
                e.field = format!("{path}.type");
                e
            })?;
        self.source_keys("image", ty, obj, path)?;
        let field = |name: &str| -> Result<&str, PolicyError> {
            want_str(
                obj.get(name).ok_or_else(|| {
                    invalid(format!("{path}.{name}"), format!("`{name}` is required"))
                })?,
                &format!("{path}.{name}"),
            )
        };
        match ty {
            "base64" => Ok(ImageSource::Base64 {
                media_type: field("media_type")?.to_owned(),
                data: field("data")?.to_owned(),
            }),
            "url" => Ok(ImageSource::Url(field("url")?.to_owned())),
            other => Err(invalid(
                format!("{path}.type"),
                format!("unsupported image source `{other}`"),
            )),
        }
    }

    /// Source keys other than the ones each source type defines are unknown fields.
    fn source_keys(
        &mut self,
        block: &str,
        ty: &str,
        obj: &Map<String, Json>,
        path: &str,
    ) -> Result<(), PolicyError> {
        let allowed: &[&str] = match (block, ty) {
            ("image", "base64") | ("document", "text") => &["type", "media_type", "data"],
            ("image", "url") => &["type", "url"],
            ("document", "content") => &["type", "content"],
            _ => &["type"],
        };
        for k in obj.keys() {
            if !allowed.contains(&k.as_str())
                && !matches!(k.as_str(), "file_id" | "url" | "media_type" | "data")
            {
                self.policy
                    .check(Group::BlockField, &format!("{block}.source.{k}"))
                    .map_err(|mut e| {
                        e.field = format!("{path}.{k}");
                        e
                    })?;
            }
        }
        Ok(())
    }

    fn document(&mut self, source: &Json, path: &str) -> Result<Vec<Part>, PolicyError> {
        let obj = want_object(source, path)?;
        let ty = want_str(
            obj.get("type")
                .ok_or_else(|| invalid(format!("{path}.type"), "`type` is required"))?,
            &format!("{path}.type"),
        )?;
        self.policy
            .check(Group::BlockField, &format!("document.source.type={ty}"))
            .map_err(|mut e| {
                e.field = format!("{path}.type");
                e
            })?;
        self.source_keys("document", ty, obj, path)?;
        match ty {
            "text" => {
                let data = want_str(
                    obj.get("data")
                        .ok_or_else(|| invalid(format!("{path}.data"), "`data` is required"))?,
                    &format!("{path}.data"),
                )?;
                Ok(vec![Part::Document(DocumentSource::Text(data.to_owned()))])
            }
            "content" => {
                let content = obj
                    .get("content")
                    .ok_or_else(|| invalid(format!("{path}.content"), "`content` is required"))?;
                match content {
                    Json::String(s) => Ok(vec![Part::Document(DocumentSource::Text(s.clone()))]),
                    Json::Array(blocks) => {
                        let mut text = String::new();
                        let mut images = Vec::new();
                        for (i, b) in blocks.iter().enumerate() {
                            for part in self.block(b, &format!("{path}.content[{i}]"))? {
                                match part {
                                    Part::Text(t) => {
                                        if !text.is_empty() {
                                            text.push('\n');
                                        }
                                        text.push_str(&t);
                                    }
                                    Part::Image(_) => images.push(part),
                                    _ => {
                                        return Err(invalid(
                                            format!("{path}.content[{i}]"),
                                            "documents may contain only text and images",
                                        ));
                                    }
                                }
                            }
                        }
                        let mut parts = vec![Part::Document(DocumentSource::Text(text))];
                        parts.extend(images);
                        Ok(parts)
                    }
                    _ => Err(invalid(
                        format!("{path}.content"),
                        "`content` must be a string or an array of blocks",
                    )),
                }
            }
            other => Err(invalid(
                format!("{path}.type"),
                format!("unsupported document source `{other}`"),
            )),
        }
    }

    fn cache_control(&mut self, value: &Json, path: &str) -> Result<(), PolicyError> {
        want_object(value, path)?;
        self.cache_hints = true;
        Ok(())
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
            let mut examples: Vec<&Json> = Vec::new();
            for (k, v) in obj {
                if self.policy.check(Group::ToolField, k)? == Decision::Skip {
                    continue;
                }
                let kp = format!("{path}.{k}");
                match k.as_str() {
                    "type" => {
                        let ty = want_str(v, &kp)?;
                        if ty != "custom" {
                            return Err(invalid(
                                kp,
                                format!("Anthropic-defined tool type `{ty}` is not supported by airmate; define a custom tool instead"),
                            ));
                        }
                    }
                    "name" => name = Some(want_str(v, &kp)?.to_owned()),
                    "description" => description = Some(want_str(v, &kp)?.to_owned()),
                    "input_schema" => {
                        want_object(v, &kp)?;
                        schema = Some(v.clone());
                    }
                    "strict" => strict = Some(want_bool(v, &kp)?),
                    "input_examples" => examples.extend(want_array(v, &kp)?),
                    "cache_control" => self.cache_control(v, &kp)?,
                    other => unhandled!("table says tool field `{other}` is translated"),
                }
            }
            let name = name.ok_or_else(|| invalid(format!("{path}.name"), "`name` is required"))?;
            if !examples.is_empty() {
                let mut d = description.unwrap_or_default();
                if !d.is_empty() {
                    d.push_str("\n\n");
                }
                d.push_str("Examples of valid input:");
                for e in examples {
                    d.push('\n');
                    d.push_str(&e.to_string());
                }
                description = Some(d);
            }
            out.push(ToolDef {
                name,
                description,
                input_schema: schema.unwrap_or_else(|| serde_json::json!({"type": "object"})),
                strict,
            });
        }
        Ok(out)
    }

    fn tool_choice(&mut self, value: &Json) -> Result<ToolChoice, PolicyError> {
        let obj = want_object(value, "tool_choice")?;
        let mut kind = None;
        let mut name = None;
        let mut parallel = true;
        for (k, v) in obj {
            if self.policy.check(Group::ToolChoiceField, k)? == Decision::Skip {
                continue;
            }
            let kp = format!("tool_choice.{k}");
            match k.as_str() {
                "type" => kind = Some(want_str(v, &kp)?),
                "name" => name = Some(want_str(v, &kp)?.to_owned()),
                "disable_parallel_tool_use" => parallel = !want_bool(v, &kp)?,
                other => unhandled!("table says tool_choice field `{other}` is translated"),
            }
        }
        let kind = match kind {
            Some("auto") => ToolChoiceKind::Auto,
            Some("any") => ToolChoiceKind::Required,
            Some("none") => ToolChoiceKind::None,
            Some("tool") => ToolChoiceKind::Named(name.ok_or_else(|| {
                invalid(
                    "tool_choice.name",
                    "`name` is required when `type` is `tool`",
                )
            })?),
            Some(other) => {
                return Err(invalid(
                    "tool_choice.type",
                    format!("unknown tool_choice type `{other}`"),
                ));
            }
            None => return Err(invalid("tool_choice.type", "`type` is required")),
        };
        Ok(ToolChoice { kind, parallel })
    }

    // ---- thinking and output_config ---------------------------------------------------------

    fn thinking(&mut self, value: &Json) -> Result<Reasoning, PolicyError> {
        let obj = want_object(value, "thinking")?;
        let mut ty = None;
        let mut budget = None;
        let mut display = ReasoningDisplay::Summarized;
        for (k, v) in obj {
            if self.policy.check(Group::ThinkingField, k)? == Decision::Skip {
                continue;
            }
            let kp = format!("thinking.{k}");
            match k.as_str() {
                "type" => {
                    let t = want_str(v, &kp)?;
                    if self.policy.check(Group::ThinkingType, t)? == Decision::Translate {
                        ty = Some(t);
                    }
                }
                "budget_tokens" => budget = Some(want_u32(v, &kp)?),
                "display" => {
                    let d = want_str(v, &kp)?;
                    if self.policy.check(Group::ThinkingDisplay, d)? == Decision::Translate {
                        display = match d {
                            "omitted" => ReasoningDisplay::Omitted,
                            "updates" => ReasoningDisplay::Updates,
                            _ => ReasoningDisplay::Summarized,
                        };
                    }
                }
                other => unhandled!("table says thinking field `{other}` is translated"),
            }
        }
        let mode = match ty {
            Some("enabled") => ReasoningMode::On {
                budget_tokens: budget,
            },
            Some("adaptive" | "between_tools") => ReasoningMode::Adaptive,
            Some("disabled") => ReasoningMode::Off,
            Some(_) | None => ReasoningMode::Unspecified,
        };
        Ok(Reasoning {
            mode,
            effort: None,
            display,
        })
    }

    fn output_config(
        &mut self,
        value: &Json,
        path: &str,
    ) -> Result<(Option<Effort>, Option<ResponseFormat>), PolicyError> {
        let obj = want_object(value, path)?;
        let mut effort = None;
        let mut format = None;
        for (k, v) in obj {
            if self.policy.check(Group::OutputConfigKey, k)? == Decision::Skip {
                continue;
            }
            let kp = format!("{path}.{k}");
            match k.as_str() {
                "effort" => {
                    let e = want_str(v, &kp)?;
                    effort = Some(Effort::parse(e).ok_or_else(|| {
                        invalid(
                            &kp,
                            format!(
                                "unknown effort `{e}`; expected low, medium, high, xhigh or max"
                            ),
                        )
                    })?);
                }
                "format" => {
                    let fo = want_object(v, &kp)?;
                    match fo.get("type").and_then(Json::as_str) {
                        Some("json_schema") => {
                            let schema = fo.get("schema").ok_or_else(|| {
                                invalid(format!("{kp}.schema"), "`schema` is required")
                            })?;
                            format = Some(ResponseFormat::JsonSchema {
                                schema: schema.clone(),
                                strict: true,
                            });
                        }
                        _ => {
                            return Err(invalid(
                                format!("{kp}.type"),
                                "`type` must be `json_schema`",
                            ))
                        }
                    }
                }
                other => unhandled!("table says output_config key `{other}` is translated"),
            }
        }
        Ok((effort, format))
    }
}

#[derive(Default)]
struct Blocks {
    parts: Vec<Part>,
    tool_results: Vec<ToolResult>,
}

/// A tool result belongs directly after the assistant turn that made the call. Client histories
/// normally already satisfy this; a result that drifted (for example behind other user text) is
/// moved back, merging with a results turn already there.
fn relocate_tool_results(turns: &mut Vec<Turn>) {
    let mut i = 0;
    while i < turns.len() {
        if !matches!(turns[i], Turn::ToolResults(_)) {
            i += 1;
            continue;
        }
        let Turn::ToolResults(results) = std::mem::replace(&mut turns[i], Turn::User(Vec::new()))
        else {
            unreachable!()
        };
        let mut stay = Vec::new();
        let mut moves: Vec<(usize, ToolResult)> = Vec::new();
        for r in results {
            let caller = turns[..i].iter().rposition(|t| match t {
                Turn::Assistant(parts) => parts
                    .iter()
                    .any(|p| matches!(p, Part::ToolCall { id, .. } if *id == r.call_id)),
                _ => false,
            });
            match caller {
                Some(c) if c + 1 != i => moves.push((c, r)),
                _ => stay.push(r),
            }
        }
        // Insert moved results, latest caller first so earlier indices stay valid.
        moves.sort_by_key(|(c, _)| std::cmp::Reverse(*c));
        let mut shift = 0;
        for (c, r) in moves {
            match turns.get_mut(c + 1) {
                Some(Turn::ToolResults(existing)) => existing.push(r),
                _ => {
                    turns.insert(c + 1, Turn::ToolResults(vec![r]));
                    shift += 1;
                }
            }
        }
        let at = i + shift;
        if stay.is_empty() {
            turns.remove(at);
        } else {
            turns[at] = Turn::ToolResults(stay);
            i = at + 1;
            continue;
        }
        i = at;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Origin;
    use crate::policy::{PolicyConfig, UnknownFields};
    use crate::signature::{sign, SignaturePayload, SigningKey};
    use serde_json::json;

    fn parse(v: Json) -> Result<Parsed<IrRequest>, PolicyError> {
        parse_request(v.to_string().as_bytes(), &InboundCtx::default())
    }

    fn ok(v: Json) -> Parsed<IrRequest> {
        parse(v).expect("parses")
    }

    fn base() -> Json {
        json!({"model": "claude-x", "max_tokens": 100, "messages": [{"role": "user", "content": "hi"}]})
    }

    fn with(extra: Json) -> Json {
        let mut b = base();
        for (k, v) in extra.as_object().unwrap() {
            b[k] = v.clone();
        }
        b
    }

    #[test]
    fn parses_a_minimal_request() {
        let p = ok(base());
        assert_eq!(p.value.model, "claude-x");
        assert_eq!(p.value.max_output_tokens, 100);
        assert_eq!(
            p.value.turns,
            vec![Turn::User(vec![Part::Text("hi".into())])]
        );
        assert!(p.ignored.is_empty());
        assert!(!p.value.stream && !p.value.cache_hints);
    }

    #[test]
    fn rejects_bodies_that_are_not_json_objects() {
        assert!(parse_request(b"nope", &InboundCtx::default())
            .unwrap_err()
            .message
            .contains("not valid JSON"));
        assert!(parse_request(b"[]", &InboundCtx::default())
            .unwrap_err()
            .message
            .contains("JSON object"));
    }

    #[test]
    fn requires_model_max_tokens_and_messages() {
        for key in ["model", "max_tokens", "messages"] {
            let mut b = base();
            b.as_object_mut().unwrap().remove(key);
            assert_eq!(parse(b).unwrap_err().field, key);
        }
    }

    #[test]
    fn count_tokens_requests_need_no_max_tokens() {
        let mut b = base();
        b.as_object_mut().unwrap().remove("max_tokens");
        let ctx = InboundCtx {
            count_tokens: true,
            ..InboundCtx::default()
        };
        let p = parse_request(b.to_string().as_bytes(), &ctx).unwrap();
        assert_eq!(p.value.max_output_tokens, 0);
        assert!(parse(b).is_err(), "an ordinary request still needs it");
    }

    #[test]
    fn rejects_zero_max_tokens() {
        let err = parse(with(json!({"max_tokens": 0}))).unwrap_err();
        assert_eq!(err.field, "max_tokens");
    }

    #[test]
    fn rejects_wrongly_typed_fields() {
        for (k, v) in [
            ("stream", json!("yes")),
            ("temperature", json!("hot")),
            ("top_k", json!(-1)),
            ("stop_sequences", json!("x")),
            ("model", json!(3)),
        ] {
            assert_eq!(parse(with(json!({ k: v }))).unwrap_err().field, k);
        }
    }

    #[test]
    fn reads_sampling_stop_and_stream() {
        let p = ok(with(
            json!({"stream": true, "temperature": 0.5, "top_p": 0.9, "top_k": 40, "stop_sequences": ["a", "b"]}),
        ));
        let ir = p.value;
        assert!(ir.stream);
        assert_eq!(ir.sampling.temperature, Some(0.5));
        assert_eq!(ir.sampling.top_p, Some(0.9));
        assert_eq!(ir.sampling.top_k, Some(40));
        assert_eq!(ir.stop, vec!["a".to_owned(), "b".to_owned()]);
    }

    #[test]
    fn rejected_fields_name_the_field() {
        for field in ["safeguards", "container", "mcp_servers"] {
            let err = parse(with(json!({ field: {} }))).unwrap_err();
            assert_eq!(err.field, field);
            assert!(err.message.contains(field));
        }
    }

    #[test]
    fn ignored_fields_are_reported() {
        let p = ok(with(
            json!({"metadata": {"user_id": "u"}, "service_tier": "auto", "context_management": {}}),
        ));
        let names: Vec<_> = p.ignored.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"metadata")
                && names.contains(&"service_tier")
                && names.contains(&"context_management")
        );
    }

    #[test]
    fn unknown_fields_are_rejected_or_warned() {
        let err = parse(with(json!({"shiny_new": 1}))).unwrap_err();
        assert!(err.message.contains("not known to airmate"));
        let ctx = InboundCtx {
            policy: PolicyConfig {
                unknown_fields: UnknownFields::Warn,
            },
            signing_key: None,
            ..InboundCtx::default()
        };
        let p = parse_request(with(json!({"shiny_new": 1})).to_string().as_bytes(), &ctx).unwrap();
        assert_eq!(p.ignored[0].name, "shiny_new");
    }

    #[test]
    fn system_as_string_and_blocks() {
        let p = ok(with(json!({"system": "be brief"})));
        assert_eq!(p.value.system[0].text, "be brief");
        assert_eq!(p.value.system[0].after_turns, 0);
        let p = ok(with(
            json!({"system": [{"type": "text", "text": "a", "cache_control": {"type": "ephemeral"}}, {"type": "text", "text": "b"}]}),
        ));
        assert_eq!(p.value.system.len(), 2);
        assert!(p.value.cache_hints);
        assert!(parse(with(json!({"system": 3}))).is_err());
        assert!(parse(with(
            json!({"system": [{"type": "image", "source": {"type": "url", "url": "u"}}]})
        ))
        .is_err());
    }

    #[test]
    fn mid_conversation_system_messages_keep_their_position_and_effort() {
        let p = ok(json!({
            "model": "m", "max_tokens": 5,
            "system": "lead",
            "messages": [
                {"role": "user", "content": "one"},
                {"role": "assistant", "content": "two"},
                {"role": "system", "content": "now think hard", "output_config": {"effort": "high"}},
                {"role": "user", "content": "three"}
            ]
        }));
        let sys = &p.value.system;
        assert_eq!((sys[0].text.as_str(), sys[0].after_turns), ("lead", 0));
        assert_eq!(
            (sys[1].text.as_str(), sys[1].after_turns, sys[1].effort),
            ("now think hard", 2, Some(Effort::High))
        );
        assert_eq!(p.value.turns.len(), 3);
    }

    #[test]
    fn a_trailing_assistant_message_is_prefill_and_reported() {
        let p = ok(json!({"model": "m", "max_tokens": 5, "messages": [
            {"role": "user", "content": "hi"}, {"role": "assistant", "content": "Sure,"}]}));
        assert!(p.ignored.iter().any(|i| i.name == "assistant_prefill"));
        assert!(matches!(p.value.turns.last(), Some(Turn::Assistant(_))));
    }

    #[test]
    fn unknown_roles_and_message_keys_are_rejected() {
        let err = parse(
            json!({"model": "m", "max_tokens": 5, "messages": [{"role": "robot", "content": "x"}]}),
        )
        .unwrap_err();
        assert_eq!(err.field, "messages[0].role");
        let err = parse(json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": "x", "name": "bob"}]})).unwrap_err();
        assert_eq!(err.field, "name");
        assert!(
            parse(json!({"model": "m", "max_tokens": 5, "messages": [{"content": "x"}]})).is_err()
        );
        assert!(
            parse(json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user"}]})).is_err()
        );
        assert!(parse(json!({"model": "m", "max_tokens": 5, "messages": ["x"]})).is_err());
        assert!(parse(
            json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": 4}]})
        )
        .is_err());
    }

    #[test]
    fn images_come_in_base64_and_url_form_and_file_is_rejected() {
        let user = |source: Json| {
            with(
                json!({"messages": [{"role": "user", "content": [{"type": "image", "source": source}]}]}),
            )
        };
        let p = ok(user(
            json!({"type": "base64", "media_type": "image/png", "data": "AAAA"}),
        ));
        assert_eq!(
            p.value.turns[0],
            Turn::User(vec![Part::Image(ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "AAAA".into()
            })])
        );
        let p = ok(user(json!({"type": "url", "url": "https://x/y.png"})));
        assert_eq!(
            p.value.turns[0],
            Turn::User(vec![Part::Image(ImageSource::Url(
                "https://x/y.png".into()
            ))])
        );
        let err = parse(user(json!({"type": "file", "file_id": "f"}))).unwrap_err();
        assert!(err.message.contains("Files API"));
        assert!(parse(user(json!({"type": "base64", "media_type": "image/png"}))).is_err());
        assert!(parse(user(json!({"type": "url", "url": "u", "extra": 1}))).is_err());
        assert!(parse(user(json!({"type": "hologram"}))).is_err());
    }

    #[test]
    fn documents_become_text_and_pdf_url_file_are_rejected() {
        let user = |source: Json| {
            with(
                json!({"messages": [{"role": "user", "content": [{"type": "document", "title": "T", "source": source}]}]}),
            )
        };
        let p = ok(user(
            json!({"type": "text", "media_type": "text/plain", "data": "body"}),
        ));
        assert_eq!(
            p.value.turns[0],
            Turn::User(vec![Part::Document(DocumentSource::Text("body".into()))])
        );
        assert!(p.ignored.iter().any(|i| i.name == "document.title"));
        let p = ok(user(
            json!({"type": "content", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]}),
        ));
        assert_eq!(
            p.value.turns[0],
            Turn::User(vec![Part::Document(DocumentSource::Text("a\nb".into()))])
        );
        let p = ok(user(json!({"type": "content", "content": "plain"})));
        assert_eq!(
            p.value.turns[0],
            Turn::User(vec![Part::Document(DocumentSource::Text("plain".into()))])
        );
        for ty in ["base64", "url", "file"] {
            assert!(parse(user(json!({"type": ty, "media_type": "application/pdf", "data": "x", "url": "u", "file_id": "f"}))).is_err(), "{ty}");
        }
    }

    #[test]
    fn search_results_and_tool_references_render_as_text() {
        let p = ok(with(json!({"messages": [{"role": "user", "content": [
            {"type": "search_result", "source": "https://a", "title": "A", "content": [{"type": "text", "text": "found"}]},
            {"type": "tool_reference", "tool_name": "grep"}
        ]}]})));
        assert_eq!(
            p.value.turns[0],
            Turn::User(vec![
                Part::Text("Source: https://a\nTitle: A\nfound".into()),
                Part::Text("Tool: grep".into()),
            ])
        );
    }

    #[test]
    fn server_tool_blocks_are_rejected() {
        for ty in [
            "server_tool_use",
            "web_search_tool_result",
            "container_upload",
        ] {
            let err = parse(with(
                json!({"messages": [{"role": "assistant", "content": [{"type": ty}]}]}),
            ))
            .unwrap_err();
            assert_eq!(err.field, ty);
        }
        assert!(parse(with(
            json!({"messages": [{"role": "user", "content": [{"type": "hologram"}]}]})
        ))
        .is_err());
        assert!(parse(with(
            json!({"messages": [{"role": "user", "content": [{"text": "no type"}]}]})
        ))
        .is_err());
    }

    #[test]
    fn unknown_block_fields_are_rejected_and_citations_ignored() {
        assert!(parse(with(json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "x", "bogus": 1}]}]}))).is_err());
        let p = ok(with(
            json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "x", "citations": {"enabled": true}}]}]}),
        ));
        assert!(p.ignored.iter().any(|i| i.name == "text.citations"));
    }

    #[test]
    fn cache_control_anywhere_sets_the_hint() {
        for v in [
            with(json!({"cache_control": {"type": "ephemeral"}})),
            with(
                json!({"messages": [{"role": "user", "content": [{"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}]}]}),
            ),
            with(
                json!({"tools": [{"name": "t", "input_schema": {}, "cache_control": {"type": "ephemeral"}}]}),
            ),
        ] {
            assert!(ok(v).value.cache_hints);
        }
    }

    #[test]
    fn tool_use_and_tool_result_round_trip() {
        let p = ok(json!({"model": "m", "max_tokens": 5, "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "read", "input": {"p": 1}}]},
            {"role": "user", "content": [
                {"type": "text", "text": "after"},
                {"type": "tool_result", "tool_use_id": "t1", "content": "file", "is_error": true}
            ]}
        ]}));
        assert_eq!(p.value.turns.len(), 4);
        assert!(
            matches!(&p.value.turns[1], Turn::Assistant(parts) if matches!(&parts[0], Part::ToolCall { id, name, .. } if id == "t1" && name == "read"))
        );
        // Tool results precede user text from the same client message.
        match &p.value.turns[2] {
            Turn::ToolResults(rs) => {
                assert_eq!(rs[0].call_id, "t1");
                assert!(rs[0].is_error);
                assert_eq!(rs[0].content, vec![Part::Text("file".into())]);
            }
            other => panic!("expected tool results, got {other:?}"),
        }
        assert_eq!(
            p.value.turns[3],
            Turn::User(vec![Part::Text("after".into())])
        );
    }

    #[test]
    fn tool_results_can_hold_blocks_and_nothing() {
        let p = ok(json!({"model": "m", "max_tokens": 5, "messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "n", "input": {}}, {"type": "tool_use", "id": "b", "name": "n", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "a", "content": [{"type": "text", "text": "x"}, {"type": "image", "source": {"type": "url", "url": "u"}}]},
                {"type": "tool_result", "tool_use_id": "b"}
            ]}
        ]}));
        match &p.value.turns[1] {
            Turn::ToolResults(rs) => {
                assert_eq!(rs.len(), 2);
                assert_eq!(rs[0].content.len(), 2);
                assert!(rs[1].content.is_empty());
            }
            other => panic!("{other:?}"),
        }
        assert!(parse(json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": [{"type": "tool_result", "content": "x"}]}]})).is_err());
        assert!(parse(json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": 3}]}]})).is_err());
    }

    #[test]
    fn a_drifted_tool_result_moves_back_next_to_its_call() {
        let p = ok(json!({"model": "m", "max_tokens": 5, "messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "n", "input": {}}]},
            {"role": "user", "content": "interjection"},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": "r"}]}
        ]}));
        assert!(matches!(p.value.turns[0], Turn::Assistant(_)));
        assert!(
            matches!(p.value.turns[1], Turn::ToolResults(_)),
            "{:?}",
            p.value.turns
        );
        assert!(matches!(p.value.turns[2], Turn::User(_)));
        assert_eq!(p.value.turns.len(), 3);
    }

    #[test]
    fn a_drifted_result_merges_into_an_existing_results_turn() {
        let p = ok(json!({"model": "m", "max_tokens": 5, "messages": [
            {"role": "assistant", "content": [{"type": "tool_use", "id": "a", "name": "n", "input": {}}, {"type": "tool_use", "id": "b", "name": "n", "input": {}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "a", "content": "ra"}]},
            {"role": "user", "content": "chatter"},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "b", "content": "rb"}]}
        ]}));
        match &p.value.turns[1] {
            Turn::ToolResults(rs) => assert_eq!(
                rs.iter().map(|r| r.call_id.as_str()).collect::<Vec<_>>(),
                ["a", "b"]
            ),
            other => panic!("{other:?}"),
        }
        assert!(matches!(p.value.turns[2], Turn::User(_)));
        assert_eq!(p.value.turns.len(), 3);
    }

    #[test]
    fn a_result_with_no_matching_call_stays_put() {
        let p = ok(json!({"model": "m", "max_tokens": 5, "messages": [
            {"role": "user", "content": "x"},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "ghost", "content": "r"}]}
        ]}));
        assert!(matches!(p.value.turns[1], Turn::ToolResults(_)));
    }

    #[test]
    fn tools_are_parsed_and_anthropic_defined_types_are_rejected() {
        let p = ok(with(json!({"tools": [
            {"name": "a", "description": "does a", "input_schema": {"type": "object", "properties": {}}, "strict": true, "type": "custom"},
            {"name": "b", "input_schema": {"type": "object"}, "input_examples": [{"x": 1}], "defer_loading": true, "eager_input_streaming": true, "allowed_callers": ["direct"]}
        ]})));
        assert_eq!(p.value.tools[0].strict, Some(true));
        assert_eq!(
            p.value.tools[1].description.as_deref(),
            Some("Examples of valid input:\n{\"x\":1}")
        );
        assert!(p.ignored.iter().any(|i| i.name == "defer_loading"));
        let err = parse(with(
            json!({"tools": [{"type": "bash_20250124", "name": "bash"}]}),
        ))
        .unwrap_err();
        assert!(err.message.contains("bash_20250124") && err.message.contains("custom tool"));
        assert!(parse(with(json!({"tools": [{"input_schema": {}}]}))).is_err());
        assert!(parse(with(json!({"tools": [{"name": "t", "input_schema": 3}]}))).is_err());
        assert!(parse(with(json!({"tools": [{"name": "t", "weird": 1}]}))).is_err());
    }

    #[test]
    fn a_tool_without_a_schema_gets_an_empty_object_schema() {
        let p = ok(with(json!({"tools": [{"name": "t"}]})));
        assert_eq!(p.value.tools[0].input_schema, json!({"type": "object"}));
    }

    #[test]
    fn tool_choice_variants() {
        let tc = |v: Json| ok(with(json!({"tool_choice": v}))).value.tool_choice;
        assert_eq!(
            tc(json!({"type": "auto"})),
            ToolChoice {
                kind: ToolChoiceKind::Auto,
                parallel: true
            }
        );
        assert_eq!(
            tc(json!({"type": "any", "disable_parallel_tool_use": true})),
            ToolChoice {
                kind: ToolChoiceKind::Required,
                parallel: false
            }
        );
        assert_eq!(tc(json!({"type": "none"})).kind, ToolChoiceKind::None);
        assert_eq!(
            tc(json!({"type": "tool", "name": "read"})).kind,
            ToolChoiceKind::Named("read".into())
        );
        assert!(parse(with(json!({"tool_choice": {"type": "tool"}}))).is_err());
        assert!(parse(with(json!({"tool_choice": {"type": "wat"}}))).is_err());
        assert!(parse(with(json!({"tool_choice": {}}))).is_err());
        assert!(parse(with(json!({"tool_choice": "auto"}))).is_err());
    }

    #[test]
    fn thinking_modes_budget_and_display() {
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
        assert_eq!(
            th(json!({"type": "between_tools"})).mode,
            ReasoningMode::Adaptive
        );
        assert_eq!(th(json!({"type": "disabled"})).mode, ReasoningMode::Off);
        assert_eq!(
            th(json!({"type": "enabled", "display": "omitted"})).display,
            ReasoningDisplay::Omitted
        );
        assert_eq!(
            th(json!({"type": "enabled", "display": "updates"})).display,
            ReasoningDisplay::Updates
        );
        assert_eq!(
            th(json!({"type": "enabled", "display": "summarized"})).display,
            ReasoningDisplay::Summarized
        );
        assert!(parse(with(json!({"thinking": {"type": "psychic"}}))).is_err());
        assert!(parse(with(
            json!({"thinking": {"type": "enabled", "display": "loud"}})
        ))
        .is_err());
        assert!(parse(with(json!({"thinking": {"type": "enabled", "mystery": 1}}))).is_err());
        assert_eq!(
            ok(with(json!({}))).value.reasoning.mode,
            ReasoningMode::Unspecified
        );
        assert_eq!(th(json!({})).mode, ReasoningMode::Unspecified);
    }

    #[test]
    fn output_config_effort_and_format() {
        let p = ok(with(
            json!({"output_config": {"effort": "xhigh", "format": {"type": "json_schema", "schema": {"type": "object"}}}}),
        ));
        assert_eq!(p.value.reasoning.effort, Some(Effort::XHigh));
        assert_eq!(
            p.value.response_format,
            ResponseFormat::JsonSchema {
                schema: json!({"type": "object"}),
                strict: true
            }
        );
        assert!(parse(with(json!({"output_config": {"effort": "extreme"}}))).is_err());
        assert!(parse(with(json!({"output_config": {"format": {"type": "xml"}}}))).is_err());
        assert!(parse(with(
            json!({"output_config": {"format": {"type": "json_schema"}}})
        ))
        .is_err());
        let err = parse(with(json!({"output_config": {"task_budget": 1}}))).unwrap_err();
        assert!(err.message.contains("task_budget"));
    }

    fn signed_history(key: &SigningKey, thinking: &str, payload: &SignaturePayload) -> Json {
        let sig = sign(key, payload, thinking);
        json!({"model": "m", "max_tokens": 5, "messages": [
            {"role": "user", "content": "q"},
            {"role": "assistant", "content": [
                {"type": "thinking", "thinking": thinking, "signature": sig},
                {"type": "text", "text": "a"}
            ]},
            {"role": "user", "content": "next"}
        ]})
    }

    #[test]
    fn verified_thinking_is_kept_with_its_origin() {
        let key = SigningKey::from_token("tok");
        let payload = SignaturePayload {
            origin: Origin {
                upstream: "fw".into(),
                protocol: Protocol::Chat,
            },
            encrypted_content: Some(vec![1, 2]),
            hidden_text: None,
        };
        let body = signed_history(&key, "hmm", &payload);
        let ctx = InboundCtx {
            policy: PolicyConfig::default(),
            signing_key: Some(&key),
            ..InboundCtx::default()
        };
        let p = parse_request(body.to_string().as_bytes(), &ctx).unwrap();
        match &p.value.turns[1] {
            Turn::Assistant(parts) => match &parts[0] {
                Part::Reasoning {
                    text,
                    origin,
                    opaque,
                    display,
                } => {
                    assert_eq!(text, "hmm");
                    assert_eq!(origin.as_ref().unwrap().upstream, "fw");
                    assert_eq!(opaque.as_deref(), Some(&[1u8, 2][..]));
                    assert_eq!(*display, ReasoningDisplay::Summarized);
                }
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        }
        assert!(p.ignored.is_empty());
    }

    #[test]
    fn omitted_thinking_recovers_its_hidden_text_from_the_signature() {
        let key = SigningKey::from_token("tok");
        let payload = SignaturePayload {
            origin: Origin {
                upstream: "fw".into(),
                protocol: Protocol::Chat,
            },
            encrypted_content: None,
            hidden_text: Some("the real thoughts".into()),
        };
        let body = signed_history(&key, "", &payload);
        let ctx = InboundCtx {
            policy: PolicyConfig::default(),
            signing_key: Some(&key),
            ..InboundCtx::default()
        };
        let p = parse_request(body.to_string().as_bytes(), &ctx).unwrap();
        match &p.value.turns[1] {
            Turn::Assistant(parts) => assert!(
                matches!(&parts[0], Part::Reasoning { text, display: ReasoningDisplay::Omitted, .. } if text == "the real thoughts")
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unverified_thinking_is_dropped_and_reported() {
        let key = SigningKey::from_token("tok");
        let payload = SignaturePayload {
            origin: Origin {
                upstream: "fw".into(),
                protocol: Protocol::Chat,
            },
            encrypted_content: None,
            hidden_text: None,
        };
        let body = signed_history(&key, "hmm", &payload);
        // Wrong key, then no key at all, then a foreign (Anthropic) signature.
        let other = SigningKey::from_token("other");
        for ctx in [
            InboundCtx {
                policy: PolicyConfig::default(),
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
        let foreign = json!({"model": "m", "max_tokens": 5, "messages": [
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "x", "signature": "EqQBCkYIAxgC"}]},
            {"role": "user", "content": "q"}]});
        let ctx = InboundCtx {
            policy: PolicyConfig::default(),
            signing_key: Some(&key),
            ..InboundCtx::default()
        };
        let p = parse_request(foreign.to_string().as_bytes(), &ctx).unwrap();
        assert!(p.ignored.iter().any(|i| i.name == "thinking_unverified"));
    }

    #[test]
    fn redacted_thinking_is_ignored() {
        let p = ok(with(json!({"messages": [
            {"role": "assistant", "content": [{"type": "redacted_thinking", "data": "xxx"}, {"type": "text", "text": "a"}]},
            {"role": "user", "content": "q"}]})));
        assert!(p.ignored.iter().any(|i| i.name == "redacted_thinking"));
        assert!(matches!(&p.value.turns[0], Turn::Assistant(parts) if parts.len() == 1));
    }

    #[test]
    fn compaction_summaries_are_kept_as_text_and_failed_ones_are_no_ops() {
        let body = |block: Json| {
            json!({"model": "m", "max_tokens": 5, "messages": [
                {"role": "assistant", "content": [block, {"type": "text", "text": "after"}]},
                {"role": "user", "content": "go on"}]})
        };
        let parse = |v: Json| parse_request(v.to_string().as_bytes(), &InboundCtx::default());
        let ok = parse(body(json!({"type": "compaction", "content": "the summary", "encrypted_content": "xyz", "signature": "s", "tool_changes": []}))).unwrap();
        let Turn::Assistant(parts) = &ok.value.turns[0] else {
            panic!("{:?}", ok.value.turns)
        };
        assert_eq!(parts[0], Part::Text("the summary".into()));
        let names: Vec<&str> = ok.ignored.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"compaction.encrypted_content")
                && names.contains(&"compaction.tool_changes"),
            "{names:?}"
        );
        let failed = parse(body(json!({"type": "compaction", "content": null}))).unwrap();
        let Turn::Assistant(parts) = &failed.value.turns[0] else {
            panic!()
        };
        assert_eq!(parts, &vec![Part::Text("after".into())]);
        assert_eq!(
            parse(body(json!({"type": "compaction", "content": ""})))
                .unwrap_err()
                .field,
            "messages[0].content[0].content"
        );
    }
}
