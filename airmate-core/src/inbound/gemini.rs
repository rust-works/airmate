//! The Google Gemini `generateContent` inbound adapter
//! (`POST /v1beta/models/{model}:generateContent` and `:streamGenerateContent`).
//!
//! The model and whether to stream come from the URL, not the body, so the caller passes them in a
//! [`Call`]. Every request field, content part and tool field is looked up in the policy table
//! ([`crate::policy::gemini::TABLE`]); the parser only handles what the table says is translated.
//! The REST API takes each field in camelCase or snake_case, and so does this parser.

use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde_json::{json, Map, Value as Json};

use super::{
    invalid, parse_object, want_array, want_bool, want_f64, want_i64, want_object, want_str,
    want_u32, InboundCtx,
};
use crate::ir::{
    DocumentSource, Effort, ImageSource, IrRequest, Part, Protocol, Reasoning, ReasoningDisplay,
    ReasoningMode, ResponseFormat, SystemPart, ToolChoiceKind, ToolDef, ToolResult, Turn,
};
use crate::policy::{gemini::TABLE, Decision, Group, Parsed, Policy, PolicyError};

/// What the URL of a Gemini request says.
#[derive(Debug, Clone, Copy)]
pub struct Call<'a> {
    /// The model in the path.
    pub model: &'a str,
    /// Whether the method is `streamGenerateContent`.
    pub stream: bool,
}

/// `snake_case` to `camelCase`; names already in camelCase are unchanged.
#[must_use]
pub fn camel(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut up = false;
    for c in key.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Parses a `generateContent` request body.
///
/// # Errors
///
/// A [`PolicyError`] naming the field when the body is malformed, a field is rejected, or a field
/// is unknown and `unknown_fields` is `reject`.
pub fn parse_request(
    body: &[u8],
    ctx: &InboundCtx<'_>,
    call: &Call<'_>,
) -> Result<Parsed<IrRequest>, PolicyError> {
    let obj = parse_object(body)?;
    let mut p = Parser {
        policy: Policy::new(TABLE, ctx.policy),
        pending: Vec::new(),
        calls: 0,
    };
    let mut ir = IrRequest::new(call.model, Protocol::Gemini);
    ir.stream = call.stream;
    p.request(&obj, &mut ir)?;
    Ok(Parsed {
        value: ir,
        ignored: p.policy.into_ignored(),
    })
}

struct Parser {
    policy: Policy,
    /// Calls the model made in the previous turn and nobody has answered: `(id, name)`.
    pending: Vec<(String, String)>,
    calls: usize,
}

fn normalised(obj: &Map<String, Json>) -> Vec<(String, &Json)> {
    obj.iter().map(|(k, v)| (camel(k), v)).collect()
}

impl Parser {
    fn request(&mut self, obj: &Map<String, Json>, ir: &mut IrRequest) -> Result<(), PolicyError> {
        let mut have_contents = false;
        for (key, value) in normalised(obj) {
            if self.policy.check(Group::Request, &key)? == Decision::Skip {
                continue;
            }
            match key.as_str() {
                "contents" => {
                    self.contents(want_array(value, "contents")?, ir)?;
                    have_contents = true;
                }
                "systemInstruction" => {
                    let text = self.system_text(value)?;
                    if !text.is_empty() {
                        ir.system.push(SystemPart {
                            text,
                            after_turns: 0,
                            effort: None,
                        });
                    }
                }
                "tools" => ir.tools = self.tools(want_array(value, "tools")?)?,
                "toolConfig" => self.tool_config(value, ir)?,
                "generationConfig" => {
                    self.generation_config(want_object(value, "generationConfig")?, ir)?
                }
                other => {
                    unreachable!("table says `{other}` is translated but the parser has no case")
                }
            }
        }
        if !have_contents || ir.turns.is_empty() {
            return Err(invalid(
                "contents",
                "`contents` is required and must not be empty",
            ));
        }
        Ok(())
    }

    // ---- system and contents ----------------------------------------------------------------

    fn system_text(&mut self, value: &Json) -> Result<String, PolicyError> {
        match value {
            Json::String(s) => Ok(s.clone()),
            Json::Object(o) => {
                let parts = o
                    .get("parts")
                    .map(|p| want_array(p, "systemInstruction.parts"))
                    .transpose()?
                    .cloned()
                    .unwrap_or_default();
                let mut texts = Vec::new();
                for (i, p) in parts.iter().enumerate() {
                    let po = want_object(p, &format!("systemInstruction.parts[{i}]"))?;
                    let t = po.get("text").ok_or_else(|| {
                        invalid(
                            format!("systemInstruction.parts[{i}]"),
                            "system instructions may contain text parts only",
                        )
                    })?;
                    texts.push(
                        want_str(t, &format!("systemInstruction.parts[{i}].text"))?.to_owned(),
                    );
                }
                Ok(texts.join("\n\n"))
            }
            _ => Err(invalid(
                "systemInstruction",
                "`systemInstruction` must be an object with `parts`",
            )),
        }
    }

    fn contents(&mut self, contents: &[Json], ir: &mut IrRequest) -> Result<(), PolicyError> {
        for (i, c) in contents.iter().enumerate() {
            let path = format!("contents[{i}]");
            let obj = want_object(c, &path)?;
            let mut role = "user";
            let mut parts: &[Json] = &[];
            for (k, v) in normalised(obj) {
                if self.policy.check(Group::MessageKey, &k)? == Decision::Skip {
                    continue;
                }
                match k.as_str() {
                    "role" => {
                        role = match want_str(v, &format!("{path}.role"))? {
                            "user" => "user",
                            "model" => "model",
                            other => {
                                return Err(invalid(
                                    format!("{path}.role"),
                                    format!("`role` must be `user` or `model`, not `{other}`"),
                                ))
                            }
                        };
                    }
                    "parts" => parts = want_array(v, &format!("{path}.parts"))?,
                    other => unreachable!("table says message key `{other}` is translated"),
                }
            }
            if role == "model" {
                let mut out = Vec::new();
                let mut calls = Vec::new();
                for (j, p) in parts.iter().enumerate() {
                    for part in self.part(p, &format!("{path}.parts[{j}]"))? {
                        if let Part::ToolCall { id, name, .. } = &part {
                            calls.push((id.clone(), name.clone()));
                        }
                        out.push(part);
                    }
                }
                self.pending = calls;
                if !out.is_empty() {
                    ir.turns.push(Turn::Assistant(out));
                }
            } else {
                let mut user = Vec::new();
                let mut results = Vec::new();
                for (j, p) in parts.iter().enumerate() {
                    let ppath = format!("{path}.parts[{j}]");
                    if p.get("functionResponse").is_some() || p.get("function_response").is_some() {
                        results.push(self.function_response(p, &ppath)?);
                    } else {
                        user.extend(self.part(p, &ppath)?);
                    }
                }
                // Tool results answer the assistant turn just before, so they come first.
                if !results.is_empty() {
                    ir.turns.push(Turn::ToolResults(results));
                }
                if !user.is_empty() {
                    ir.turns.push(Turn::User(user));
                }
                self.pending.clear();
            }
        }
        Ok(())
    }

    fn part(&mut self, p: &Json, path: &str) -> Result<Vec<Part>, PolicyError> {
        let obj = want_object(p, path)?;
        let mut thought = false;
        let mut out = Vec::new();
        let mut content: Option<(&str, &Json)> = None;
        for (k, v) in normalised(obj) {
            let group_key = match k.as_str() {
                "thought" => (Group::BlockField, "text.thought"),
                "thoughtSignature" | "videoMetadata" => (Group::BlockField, k.as_str()),
                _ => (Group::ContentBlock, k.as_str()),
            };
            if self.policy.check(group_key.0, group_key.1)? == Decision::Skip {
                if k == "thought" && v.as_bool() == Some(true) {
                    thought = true;
                }
                continue;
            }
            content = Some((
                match k.as_str() {
                    "text" => "text",
                    "inlineData" => "inlineData",
                    "functionCall" => "functionCall",
                    other => unreachable!("table says part `{other}` is translated"),
                },
                v,
            ));
        }
        let Some((kind, v)) = content else {
            return Ok(out);
        };
        match kind {
            "text" => {
                // A thought summary from an earlier turn is not replayed.
                if !thought {
                    out.push(Part::Text(want_str(v, &format!("{path}.text"))?.to_owned()));
                }
            }
            "inlineData" => out.push(self.inline_data(v, path)?),
            "functionCall" => {
                let o = want_object(v, &format!("{path}.functionCall"))?;
                let name = want_str(
                    o.get("name").ok_or_else(|| {
                        invalid(format!("{path}.functionCall.name"), "`name` is required")
                    })?,
                    &format!("{path}.functionCall.name"),
                )?
                .to_owned();
                self.calls += 1;
                let id = o
                    .get("id")
                    .and_then(Json::as_str)
                    .map_or_else(|| format!("call_{}", self.calls), str::to_owned);
                let input = o.get("args").cloned().unwrap_or_else(|| json!({}));
                out.push(Part::ToolCall { id, name, input });
            }
            _ => {}
        }
        Ok(out)
    }

    fn inline_data(&mut self, v: &Json, path: &str) -> Result<Part, PolicyError> {
        let o = want_object(v, &format!("{path}.inlineData"))?;
        let mime = want_str(
            o.get("mimeType")
                .or_else(|| o.get("mime_type"))
                .ok_or_else(|| {
                    invalid(
                        format!("{path}.inlineData.mimeType"),
                        "`mimeType` is required",
                    )
                })?,
            &format!("{path}.inlineData.mimeType"),
        )?;
        let data = want_str(
            o.get("data")
                .ok_or_else(|| invalid(format!("{path}.inlineData.data"), "`data` is required"))?,
            &format!("{path}.inlineData.data"),
        )?;
        if mime.starts_with("image/") {
            Ok(Part::Image(ImageSource::Base64 {
                media_type: mime.to_owned(),
                data: data.to_owned(),
            }))
        } else if mime == "application/pdf" {
            Ok(Part::Document(DocumentSource::Pdf(data.to_owned())))
        } else if mime.starts_with("text/") {
            let bytes = STANDARD.decode(data).map_err(|_| {
                invalid(
                    format!("{path}.inlineData.data"),
                    "`data` is not valid base64",
                )
            })?;
            let text = String::from_utf8(bytes).map_err(|_| {
                invalid(
                    format!("{path}.inlineData.data"),
                    "text data is not valid UTF-8",
                )
            })?;
            Ok(Part::Document(DocumentSource::Text(text)))
        } else {
            Err(invalid(
                format!("{path}.inlineData.mimeType"),
                format!("`{mime}` is not supported: send images, PDFs or text"),
            ))
        }
    }

    fn function_response(&mut self, p: &Json, path: &str) -> Result<ToolResult, PolicyError> {
        let obj = want_object(p, path)?;
        let fr = obj
            .get("functionResponse")
            .or_else(|| obj.get("function_response"))
            .ok_or_else(|| invalid(path, "`functionResponse` is required"))?;
        let fpath = format!("{path}.functionResponse");
        let o = want_object(fr, &fpath)?;
        let name = want_str(
            o.get("name")
                .ok_or_else(|| invalid(format!("{fpath}.name"), "`name` is required"))?,
            &format!("{fpath}.name"),
        )?;
        // A response with no id answers the first unanswered call of that name.
        let call_id = match o.get("id").and_then(Json::as_str) {
            Some(id) => id.to_owned(),
            None => {
                let at = self.pending.iter().position(|(_, n)| n == name);
                match at {
                    Some(i) => self.pending.remove(i).0,
                    None => {
                        return Err(invalid(
                            format!("{fpath}.name"),
                            format!("the function response for `{name}` answers no function call"),
                        ))
                    }
                }
            }
        };
        let response = o.get("response").cloned().unwrap_or(Json::Null);
        let (text, is_error) = match &response {
            Json::Object(r) if r.get("error").is_some() && r.len() == 1 => {
                (json_text(&r["error"]), true)
            }
            Json::Object(r) if r.get("output").is_some() && r.len() == 1 => {
                (json_text(&r["output"]), false)
            }
            other => (json_text(other), false),
        };
        Ok(ToolResult {
            call_id,
            content: vec![Part::Text(text)],
            is_error,
        })
    }

    // ---- tools ------------------------------------------------------------------------------

    fn tools(&mut self, tools: &[Json]) -> Result<Vec<ToolDef>, PolicyError> {
        let mut out = Vec::new();
        for (i, t) in tools.iter().enumerate() {
            let path = format!("tools[{i}]");
            let obj = want_object(t, &path)?;
            for (k, v) in normalised(obj) {
                if self.policy.check(Group::ToolField, &k)? == Decision::Skip {
                    continue;
                }
                if k != "functionDeclarations" {
                    unreachable!("table says tool field `{k}` is translated");
                }
                for (j, d) in want_array(v, &format!("{path}.functionDeclarations"))?
                    .iter()
                    .enumerate()
                {
                    out.push(self.declaration(d, &format!("{path}.functionDeclarations[{j}]"))?);
                }
            }
        }
        Ok(out)
    }

    fn declaration(&mut self, d: &Json, path: &str) -> Result<ToolDef, PolicyError> {
        let obj = want_object(d, path)?;
        let mut name = None;
        let mut description = None;
        let mut schema = None;
        for (k, v) in normalised(obj) {
            if self.policy.check(Group::ToolField, &k)? == Decision::Skip {
                continue;
            }
            match k.as_str() {
                "name" => name = Some(want_str(v, &format!("{path}.name"))?.to_owned()),
                "description" => {
                    description = Some(want_str(v, &format!("{path}.description"))?.to_owned())
                }
                "parameters" => schema = Some(schema_from_openapi(v)),
                "parametersJsonSchema" => {
                    if schema.is_none() {
                        schema = Some(v.clone());
                    }
                }
                other => unreachable!("table says declaration field `{other}` is translated"),
            }
        }
        Ok(ToolDef {
            name: name.ok_or_else(|| invalid(format!("{path}.name"), "`name` is required"))?,
            description,
            input_schema: schema.unwrap_or_else(|| json!({"type": "object", "properties": {}})),
            strict: None,
        })
    }

    fn tool_config(&mut self, value: &Json, ir: &mut IrRequest) -> Result<(), PolicyError> {
        let obj = want_object(value, "toolConfig")?;
        for (k, v) in normalised(obj) {
            if k != "functionCallingConfig" {
                self.policy
                    .ignore(&format!("toolConfig.{k}"), "no upstream equivalent");
                continue;
            }
            let cfg = want_object(v, "toolConfig.functionCallingConfig")?;
            let mut mode = "AUTO".to_owned();
            let mut allowed: Vec<String> = Vec::new();
            for (ck, cv) in normalised(cfg) {
                if self.policy.check(Group::ToolChoiceField, &ck)? == Decision::Skip {
                    continue;
                }
                match ck.as_str() {
                    "mode" => {
                        mode = want_str(cv, "toolConfig.functionCallingConfig.mode")?.to_owned()
                    }
                    "allowedFunctionNames" => {
                        allowed = want_array(
                            cv,
                            "toolConfig.functionCallingConfig.allowedFunctionNames",
                        )?
                        .iter()
                        .map(|n| want_str(n, "allowedFunctionNames[]").map(str::to_owned))
                        .collect::<Result<_, _>>()?;
                    }
                    other => unreachable!("table says tool choice field `{other}` is translated"),
                }
            }
            ir.tool_choice.kind = match mode.as_str() {
                "AUTO" | "VALIDATED" | "MODE_UNSPECIFIED" => ToolChoiceKind::Auto,
                "NONE" => ToolChoiceKind::None,
                "ANY" => match allowed.as_slice() {
                    [one] => ToolChoiceKind::Named(one.clone()),
                    [] => ToolChoiceKind::Required,
                    _ => {
                        self.policy.ignore(
                            "allowedFunctionNames",
                            "airmate can require a tool call but not restrict it to several functions",
                        );
                        ToolChoiceKind::Required
                    }
                },
                other => {
                    return Err(invalid(
                        "toolConfig.functionCallingConfig.mode",
                        format!("unknown mode `{other}`; expected AUTO, ANY, NONE or VALIDATED"),
                    ))
                }
            };
        }
        Ok(())
    }

    // ---- generationConfig -------------------------------------------------------------------

    fn generation_config(
        &mut self,
        cfg: &Map<String, Json>,
        ir: &mut IrRequest,
    ) -> Result<(), PolicyError> {
        let mut mime: Option<&str> = None;
        let mut schema: Option<Json> = None;
        for (k, v) in normalised(cfg) {
            let name = format!("generationConfig.{k}");
            if self.policy.check(Group::Request, &name)? == Decision::Skip {
                continue;
            }
            match k.as_str() {
                "temperature" => ir.sampling.temperature = Some(want_f64(v, &name)?),
                "topP" => ir.sampling.top_p = Some(want_f64(v, &name)?),
                "topK" => ir.sampling.top_k = Some(want_u32(v, &name)?),
                "seed" => ir.sampling.seed = Some(want_i64(v, &name)?),
                "presencePenalty" => ir.sampling.presence_penalty = Some(want_f64(v, &name)?),
                "frequencyPenalty" => ir.sampling.frequency_penalty = Some(want_f64(v, &name)?),
                "candidateCount" => {
                    if want_u32(v, &name)? != 1 {
                        return Err(invalid(
                            name,
                            "`candidateCount` must be 1: airmate returns one candidate",
                        ));
                    }
                }
                "maxOutputTokens" => {
                    ir.max_output_tokens = want_u32(v, &name)?;
                    if ir.max_output_tokens == 0 {
                        return Err(invalid(
                            name,
                            "the output token limit must be greater than 0",
                        ));
                    }
                }
                "stopSequences" => {
                    ir.stop = want_array(v, &name)?
                        .iter()
                        .map(|s| want_str(s, "stopSequences[]").map(str::to_owned))
                        .collect::<Result<_, _>>()?;
                }
                "responseMimeType" => mime = Some(want_str(v, &name)?),
                "responseSchema" => schema = Some(schema_from_openapi(v)),
                "responseJsonSchema" => schema = Some(v.clone()),
                "responseModalities" => {
                    let all_text = want_array(v, &name)?
                        .iter()
                        .all(|m| m.as_str() == Some("TEXT"));
                    if !all_text {
                        return Err(invalid(name, "only `TEXT` output is supported"));
                    }
                }
                "thinkingConfig" => ir.reasoning = self.thinking(want_object(v, &name)?)?,
                other => {
                    unreachable!("table says `{other}` is translated but the parser has no case")
                }
            }
        }
        ir.response_format = match (mime, schema) {
            (Some("application/json"), Some(schema)) | (None, Some(schema)) => {
                ResponseFormat::JsonSchema {
                    schema,
                    strict: false,
                }
            }
            (Some("application/json"), None) => ResponseFormat::JsonObject,
            (Some("text/plain") | None, None) => ResponseFormat::Text,
            (Some(other), _) => {
                return Err(invalid(
                    "generationConfig.responseMimeType",
                    format!("`{other}` is not supported: use `application/json` or `text/plain`"),
                ))
            }
        };
        Ok(())
    }

    fn thinking(&mut self, cfg: &Map<String, Json>) -> Result<Reasoning, PolicyError> {
        let mut budget: Option<i64> = None;
        let mut effort = None;
        let mut include = false;
        for (k, v) in normalised(cfg) {
            if self.policy.check(Group::ThinkingField, &k)? == Decision::Skip {
                continue;
            }
            let path = format!("generationConfig.thinkingConfig.{k}");
            match k.as_str() {
                "includeThoughts" => include = want_bool(v, &path)?,
                "thinkingBudget" => budget = Some(want_i64(v, &path)?),
                "thinkingLevel" => {
                    effort = Some(match want_str(v, &path)?.to_ascii_lowercase().as_str() {
                        "minimal" | "low" => Effort::Low,
                        "medium" => Effort::Medium,
                        "high" => Effort::High,
                        other => {
                            return Err(invalid(path, format!("unknown thinking level `{other}`")))
                        }
                    });
                }
                other => unreachable!("table says thinking field `{other}` is translated"),
            }
        }
        let mode = match budget {
            Some(0) => ReasoningMode::Off,
            Some(-1) => ReasoningMode::Adaptive,
            Some(b) => ReasoningMode::On {
                budget_tokens: Some(u32::try_from(b).map_err(|_| {
                    invalid(
                        "generationConfig.thinkingConfig.thinkingBudget",
                        "`thinkingBudget` must be -1 or non-negative",
                    )
                })?),
            },
            None if effort.is_some() || include => ReasoningMode::Adaptive,
            None => ReasoningMode::Unspecified,
        };
        Ok(Reasoning {
            mode,
            effort,
            // Thought summaries are returned only when the client asks for them.
            display: if include {
                ReasoningDisplay::Summarized
            } else {
                ReasoningDisplay::Omitted
            },
        })
    }
}

fn json_text(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Null => String::new(),
        other => other.to_string(),
    }
}

/// Converts Gemini's OpenAPI-subset `Schema` into JSON Schema: type names in lower case, the
/// integer-string bounds as numbers, and the Gemini-only keys dropped.
#[must_use]
pub fn schema_from_openapi(v: &Json) -> Json {
    match v {
        Json::Object(o) => {
            let mut out = Map::new();
            for (k, val) in o {
                match k.as_str() {
                    "propertyOrdering" | "property_ordering" | "nullable" => {}
                    "type" => {
                        out.insert(
                            k.clone(),
                            match val.as_str() {
                                Some(t) => Json::String(t.to_ascii_lowercase()),
                                None => val.clone(),
                            },
                        );
                    }
                    "minItems" | "maxItems" | "minLength" | "maxLength" | "minProperties"
                    | "maxProperties" => {
                        let n = val.as_str().and_then(|s| s.parse::<u64>().ok());
                        out.insert(k.clone(), n.map_or_else(|| val.clone(), |n| json!(n)));
                    }
                    "properties" => {
                        let props = val.as_object().map(|p| {
                            p.iter()
                                .map(|(pk, pv)| (pk.clone(), schema_from_openapi(pv)))
                                .collect::<Map<_, _>>()
                        });
                        out.insert(k.clone(), props.map_or_else(|| val.clone(), Json::Object));
                    }
                    "items" | "additionalProperties" => {
                        out.insert(k.clone(), schema_from_openapi(val));
                    }
                    "anyOf" | "oneOf" | "allOf" => {
                        let list = val
                            .as_array()
                            .map(|a| a.iter().map(schema_from_openapi).collect());
                        out.insert(k.clone(), list.map_or_else(|| val.clone(), Json::Array));
                    }
                    _ => {
                        out.insert(k.clone(), val.clone());
                    }
                }
            }
            Json::Object(out)
        }
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(v: &Json) -> Result<Parsed<IrRequest>, PolicyError> {
        parse_request(
            v.to_string().as_bytes(),
            &InboundCtx::default(),
            &Call {
                model: "gemini-2.5-pro",
                stream: false,
            },
        )
    }

    fn ok(v: &Json) -> IrRequest {
        parse(v).unwrap().value
    }

    fn err(v: &Json) -> PolicyError {
        parse(v).unwrap_err()
    }

    #[test]
    fn a_minimal_request_takes_its_model_and_stream_from_the_url() {
        let ir = parse_request(
            br#"{"contents":[{"role":"user","parts":[{"text":"hi"}]}]}"#,
            &InboundCtx::default(),
            &Call {
                model: "gemini-2.5-flash",
                stream: true,
            },
        )
        .unwrap()
        .value;
        assert_eq!(
            (ir.model.as_str(), ir.stream, ir.extensions.inbound),
            ("gemini-2.5-flash", true, Protocol::Gemini)
        );
        assert_eq!(ir.turns, vec![Turn::User(vec![Part::Text("hi".into())])]);
        assert_eq!(ir.max_output_tokens, 0, "no limit from the client");
    }

    #[test]
    fn snake_case_and_camel_case_are_both_accepted() {
        let ir = ok(&json!({
            "system_instruction": {"parts": [{"text": "be brief"}]},
            "contents": [{"parts": [{"text": "hi"}]}],
            "generation_config": {"max_output_tokens": 50, "top_p": 0.5, "stop_sequences": ["X"]}
        }));
        assert_eq!(ir.system[0].text, "be brief");
        assert_eq!(
            (ir.max_output_tokens, ir.sampling.top_p, ir.stop.clone()),
            (50, Some(0.5), vec!["X".to_owned()])
        );
        assert_eq!(camel("max_output_tokens"), "maxOutputTokens");
        assert_eq!(camel("topK"), "topK");
    }

    #[test]
    fn generation_config_maps_to_the_ir() {
        let ir = ok(&json!({
            "contents": [{"parts": [{"text": "hi"}]}],
            "generationConfig": {"temperature": 0.3, "topK": 20, "seed": 9, "presencePenalty": 0.1, "frequencyPenalty": 0.2, "candidateCount": 1, "responseModalities": ["TEXT"], "maxOutputTokens": 100}
        }));
        assert_eq!(ir.sampling.temperature, Some(0.3));
        assert_eq!((ir.sampling.top_k, ir.sampling.seed), (Some(20), Some(9)));
        assert_eq!(
            (ir.sampling.presence_penalty, ir.sampling.frequency_penalty),
            (Some(0.1), Some(0.2))
        );
        assert_eq!(ir.max_output_tokens, 100);
    }

    #[test]
    fn structured_output_comes_from_the_mime_type_and_schema() {
        let base =
            |cfg: Json| json!({"contents": [{"parts": [{"text": "hi"}]}], "generationConfig": cfg});
        assert_eq!(
            ok(&base(json!({"responseMimeType": "application/json"}))).response_format,
            ResponseFormat::JsonObject
        );
        assert_eq!(
            ok(&base(json!({"responseMimeType": "text/plain"}))).response_format,
            ResponseFormat::Text
        );
        let r = ok(&base(
            json!({"responseMimeType": "application/json", "responseSchema": {"type": "OBJECT", "properties": {"n": {"type": "INTEGER", "nullable": true}}, "propertyOrdering": ["n"]}}),
        ));
        assert_eq!(
            r.response_format,
            ResponseFormat::JsonSchema {
                schema: json!({"type": "object", "properties": {"n": {"type": "integer"}}}),
                strict: false
            }
        );
        let j = ok(&base(json!({"responseJsonSchema": {"type": "object"}})));
        assert!(matches!(
            j.response_format,
            ResponseFormat::JsonSchema { .. }
        ));
        assert_eq!(
            err(&base(json!({"responseMimeType": "text/x.enum"}))).field,
            "generationConfig.responseMimeType"
        );
    }

    #[test]
    fn thinking_config_maps_budgets_levels_and_visibility() {
        let r = |t: Json| {
            ok(&json!({"contents": [{"parts": [{"text": "x"}]}], "generationConfig": {"thinkingConfig": t}})).reasoning
        };
        let on = r(json!({"thinkingBudget": 2048, "includeThoughts": true}));
        assert_eq!(
            (on.mode, on.display),
            (
                ReasoningMode::On {
                    budget_tokens: Some(2048)
                },
                ReasoningDisplay::Summarized
            )
        );
        assert_eq!(r(json!({"thinkingBudget": 0})).mode, ReasoningMode::Off);
        assert_eq!(
            r(json!({"thinkingBudget": -1})).mode,
            ReasoningMode::Adaptive
        );
        let lvl = r(json!({"thinkingLevel": "HIGH"}));
        assert_eq!(
            (lvl.mode, lvl.effort),
            (ReasoningMode::Adaptive, Some(Effort::High))
        );
        assert_eq!(
            r(json!({"thinkingLevel": "minimal"})).effort,
            Some(Effort::Low)
        );
        assert_eq!(
            r(json!({"thinkingBudget": 100})).display,
            ReasoningDisplay::Omitted,
            "thoughts are shown only on request"
        );
        let e = parse(&json!({"contents": [{"parts": [{"text": "x"}]}], "generationConfig": {"thinkingConfig": {"thinkingBudget": -5}}})).unwrap_err();
        assert_eq!(e.field, "generationConfig.thinkingConfig.thinkingBudget");
    }

    #[test]
    fn a_conversation_with_a_tool_round_trip_becomes_turns() {
        let ir = ok(&json!({
            "contents": [
                {"role": "user", "parts": [{"text": "weather?"}]},
                {"role": "model", "parts": [{"text": "checking"}, {"functionCall": {"name": "get_weather", "args": {"city": "Paris"}}}]},
                {"role": "user", "parts": [{"functionResponse": {"name": "get_weather", "response": {"output": "sunny"}}}, {"text": "thanks"}]}
            ],
            "tools": [{"functionDeclarations": [{"name": "get_weather", "description": "w", "parameters": {"type": "OBJECT", "properties": {"city": {"type": "STRING"}}}}]}],
            "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["get_weather"]}}
        }));
        assert_eq!(ir.turns.len(), 4);
        let Turn::Assistant(parts) = &ir.turns[1] else {
            panic!()
        };
        assert_eq!(
            parts[1],
            Part::ToolCall {
                id: "call_1".into(),
                name: "get_weather".into(),
                input: json!({"city": "Paris"})
            }
        );
        assert_eq!(
            ir.turns[2],
            Turn::ToolResults(vec![ToolResult {
                call_id: "call_1".into(),
                content: vec![Part::Text("sunny".into())],
                is_error: false
            }]),
            "an unanswered call of that name is matched by name"
        );
        assert_eq!(ir.turns[3], Turn::User(vec![Part::Text("thanks".into())]));
        assert_eq!(
            ir.tools[0].input_schema,
            json!({"type": "object", "properties": {"city": {"type": "string"}}})
        );
        assert_eq!(
            ir.tool_choice.kind,
            ToolChoiceKind::Named("get_weather".into())
        );
    }

    #[test]
    fn ids_are_used_when_the_client_sends_them_and_errors_are_flagged() {
        let ir = ok(&json!({
            "contents": [
                {"role": "user", "parts": [{"text": "go"}]},
                {"role": "model", "parts": [{"functionCall": {"id": "x9", "name": "t", "args": {}}}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "x9", "name": "t", "response": {"error": "nope"}}}]}
            ]
        }));
        assert_eq!(
            ir.turns[2],
            Turn::ToolResults(vec![ToolResult {
                call_id: "x9".into(),
                content: vec![Part::Text("nope".into())],
                is_error: true
            }])
        );
        let other = ok(&json!({"contents": [
            {"role": "user", "parts": [{"text": "go"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "t", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "t", "response": {"temp": 21}}}]}
        ]}));
        assert_eq!(
            other.turns[2],
            Turn::ToolResults(vec![ToolResult {
                call_id: "call_1".into(),
                content: vec![Part::Text("{\"temp\":21}".into())],
                is_error: false
            }])
        );
    }

    #[test]
    fn a_function_response_with_nothing_to_answer_is_refused() {
        let e = err(
            &json!({"contents": [{"role": "user", "parts": [{"functionResponse": {"name": "ghost", "response": {}}}]}]}),
        );
        assert!(
            e.message.contains("answers no function call"),
            "{}",
            e.message
        );
    }

    #[test]
    fn inline_data_becomes_images_pdfs_and_text() {
        let ir = ok(&json!({"contents": [{"parts": [
            {"inlineData": {"mimeType": "image/png", "data": "AAAA"}},
            {"inline_data": {"mime_type": "application/pdf", "data": "JVBER"}},
            {"inlineData": {"mimeType": "text/plain", "data": "aGVsbG8="}}
        ]}]}));
        assert_eq!(
            ir.turns[0],
            Turn::User(vec![
                Part::Image(ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "AAAA".into()
                }),
                Part::Document(DocumentSource::Pdf("JVBER".into())),
                Part::Document(DocumentSource::Text("hello".into())),
            ])
        );
        let e = err(
            &json!({"contents": [{"parts": [{"inlineData": {"mimeType": "audio/mp3", "data": "AA"}}]}]}),
        );
        assert!(e.field.ends_with("inlineData.mimeType"), "{}", e.field);
    }

    #[test]
    fn thought_parts_and_signatures_from_earlier_turns_are_dropped_and_reported() {
        let p = parse(&json!({"contents": [
            {"role": "user", "parts": [{"text": "q"}]},
            {"role": "model", "parts": [{"text": "mulling", "thought": true}, {"text": "answer", "thoughtSignature": "AQID"}]}
        ]}))
        .unwrap();
        assert_eq!(
            p.value.turns[1],
            Turn::Assistant(vec![Part::Text("answer".into())])
        );
        let names: Vec<&str> = p.ignored.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"text.thought") && names.contains(&"thoughtSignature"),
            "{names:?}"
        );
    }

    #[test]
    fn rejected_and_unknown_fields_are_named() {
        let c = json!([{"parts": [{"text": "x"}]}]);
        for (extra, field) in [
            (
                json!({"cachedContent": "cachedContents/1"}),
                "cachedContent",
            ),
            (json!({"tools": [{"googleSearch": {}}]}), "googleSearch"),
            (json!({"tools": [{"codeExecution": {}}]}), "codeExecution"),
            (
                json!({"generationConfig": {"responseLogprobs": true}}),
                "generationConfig.responseLogprobs",
            ),
            (
                json!({"generationConfig": {"candidateCount": 2}}),
                "generationConfig.candidateCount",
            ),
            (
                json!({"generationConfig": {"responseModalities": ["IMAGE"]}}),
                "generationConfig.responseModalities",
            ),
            (json!({"surprise": 1}), "surprise"),
            (
                json!({"generationConfig": {"surprise": 1}}),
                "generationConfig.surprise",
            ),
        ] {
            let mut body = json!({ "contents": c.clone() });
            body.as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            assert_eq!(err(&body).field, field, "{body}");
        }
        let file = err(
            &json!({"contents": [{"parts": [{"fileData": {"fileUri": "gs://b/o", "mimeType": "image/png"}}]}]}),
        );
        assert_eq!(file.field, "fileData");
        assert_eq!(err(&json!({"contents": []})).field, "contents");
    }

    #[test]
    fn ignored_fields_are_reported_not_refused() {
        let p = parse(&json!({"contents": [{"parts": [{"text": "x"}]}], "safetySettings": [{"category": "HARM_CATEGORY_HARASSMENT", "threshold": "BLOCK_NONE"}], "labels": {"a": "b"}})).unwrap();
        let mut names: Vec<&str> = p.ignored.iter().map(|i| i.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, ["labels", "safetySettings"]);
    }

    #[test]
    fn malformed_bodies_are_refused() {
        assert_eq!(err(&json!({})).field, "contents");
        assert_eq!(
            err(&json!({"contents": [{"role": "robot", "parts": []}]})).field,
            "contents[0].role"
        );
        assert_eq!(
            parse_request(
                b"nope",
                &InboundCtx::default(),
                &Call {
                    model: "m",
                    stream: false
                }
            )
            .unwrap_err()
            .field,
            "body"
        );
        assert_eq!(
            err(&json!({"contents": [{"parts": [{"text": 5}]}]})).field,
            "contents[0].parts[0].text"
        );
    }

    #[test]
    fn several_allowed_functions_force_a_call_and_say_so() {
        let p = parse(&json!({"contents": [{"parts": [{"text": "x"}]}], "toolConfig": {"functionCallingConfig": {"mode": "ANY", "allowedFunctionNames": ["a", "b"]}}})).unwrap();
        assert_eq!(p.value.tool_choice.kind, ToolChoiceKind::Required);
        assert!(p.ignored.iter().any(|i| i.name == "allowedFunctionNames"));
        let none = ok(
            &json!({"contents": [{"parts": [{"text": "x"}]}], "toolConfig": {"functionCallingConfig": {"mode": "NONE"}}}),
        );
        assert_eq!(none.tool_choice.kind, ToolChoiceKind::None);
    }

    #[test]
    fn schemas_lose_their_gemini_only_dialect() {
        let s = schema_from_openapi(&json!({
            "type": "ARRAY", "minItems": "1", "maxItems": "3",
            "items": {"type": "OBJECT", "nullable": true, "properties": {"a": {"type": "NUMBER"}}, "anyOf": [{"type": "STRING"}]}
        }));
        assert_eq!(
            s,
            json!({"type": "array", "minItems": 1, "maxItems": 3, "items": {"type": "object", "properties": {"a": {"type": "number"}}, "anyOf": [{"type": "string"}]}})
        );
    }
}
