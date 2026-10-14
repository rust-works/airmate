//! Passthrough for the OpenAI protocols.
//!
//! When the client and the upstream speak the same protocol the body is forwarded as the client
//! sent it: the model is mapped, and the reply echoes the model the client named. For streams the
//! `model` field of every chunk (Chat) or `response` object (Responses) is rewritten. A stream
//! that ends without its terminal event ends with an error instead.

use serde_json::{json, Map, Value as Json};

use super::{Target, UpstreamRequest};
use crate::ir::{Protocol, Usage};
use crate::policy::PolicyError;
use crate::sse::{format_event, SseEvent, SseParser};
use crate::stream::StreamError;
use crate::toolname::ToolNames;

fn obj(body: &[u8]) -> Result<Map<String, Json>, PolicyError> {
    super::anthropic::parse(body)
}

/// Builds the forwarded request: the model mapped, and for streams usage requested so the call can
/// be accounted for. `client_wants_usage` tells the rewriter whether to hide the usage chunk again.
///
/// # Errors
///
/// The body is not a JSON object.
pub fn build(
    body: &[u8],
    target: &Target,
    protocol: Protocol,
) -> Result<(UpstreamRequest, bool), PolicyError> {
    let mut o = obj(body)?;
    o.insert("model".to_owned(), Json::String(target.model.clone()));
    let stream = o.get("stream").and_then(Json::as_bool).unwrap_or(false);
    let mut client_wants_usage = true;
    if protocol == Protocol::Chat && stream {
        let so = o.entry("stream_options").or_insert_with(|| json!({}));
        if let Some(m) = so.as_object_mut() {
            client_wants_usage = m
                .get("include_usage")
                .and_then(Json::as_bool)
                .unwrap_or(false);
            m.insert("include_usage".to_owned(), Json::Bool(true));
        }
    }
    let path = match protocol {
        Protocol::Responses => super::responses::PATH,
        _ => super::chat::PATH,
    };
    Ok((
        UpstreamRequest {
            path: path.to_owned(),
            body: serde_json::to_vec(&Json::Object(o)).expect("a JSON value serialises"),
            stream,
            tool_names: ToolNames::new(),
            ignored: Vec::new(),
        },
        client_wants_usage,
    ))
}

/// Rewrites a non-streaming reply so it echoes the client's model name, and reads its usage.
/// A body that is not JSON is returned unchanged.
#[must_use]
pub fn rewrite_response(body: &[u8], client_model: &str, protocol: Protocol) -> (Vec<u8>, Usage) {
    let Ok(Json::Object(mut o)) = serde_json::from_slice::<Json>(body) else {
        return (body.to_vec(), Usage::default());
    };
    let usage = usage_of(&Json::Object(o.clone()), protocol);
    if o.contains_key("model") {
        o.insert("model".to_owned(), Json::String(client_model.to_owned()));
    }
    (
        serde_json::to_vec(&Json::Object(o)).expect("a JSON value serialises"),
        usage,
    )
}

fn usage_of(v: &Json, protocol: Protocol) -> Usage {
    match protocol {
        Protocol::Responses => v
            .get("response")
            .unwrap_or(v)
            .get("usage")
            .map(super::responses_parse::usage)
            .unwrap_or_default(),
        _ => v
            .get("usage")
            .filter(|u| u.is_object())
            .map(super::chat_parse::usage)
            .unwrap_or_default(),
    }
}

/// Re-frames a passthrough stream.
#[derive(Debug)]
pub struct JsonPassthroughStream {
    sse: SseParser,
    protocol: Protocol,
    client_model: String,
    hide_usage_chunk: bool,
    terminal: bool,
    errored: bool,
    usage: Usage,
}

impl JsonPassthroughStream {
    /// A rewriter echoing `client_model`. `client_wants_usage` is what [`build`] returned.
    #[must_use]
    pub fn new(protocol: Protocol, client_model: &str, client_wants_usage: bool) -> Self {
        Self {
            sse: SseParser::new(),
            protocol,
            client_model: client_model.to_owned(),
            hide_usage_chunk: protocol == Protocol::Chat && !client_wants_usage,
            terminal: false,
            errored: false,
            usage: Usage::default(),
        }
    }

    /// The usage the stream reported.
    #[must_use]
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// Whether the stream ended properly.
    #[must_use]
    pub fn completed(&self) -> bool {
        self.terminal && !self.errored
    }

    fn handle(&mut self, events: Vec<SseEvent>) -> Vec<u8> {
        let mut out = Vec::new();
        for ev in events {
            let name = ev.event.clone();
            if ev.data.trim() == "[DONE]" {
                self.terminal = true;
                out.extend(format_event(name.as_deref(), "[DONE]"));
                continue;
            }
            let mut data = ev.data;
            if let Ok(mut json) = serde_json::from_str::<Json>(&data) {
                match self.protocol {
                    Protocol::Responses => {
                        let ty = json.get("type").and_then(Json::as_str).map(str::to_owned);
                        if let Some(m) = json.pointer_mut("/response/model") {
                            *m = Json::String(self.client_model.clone());
                        }
                        match ty.as_deref() {
                            Some("response.completed" | "response.incomplete") => {
                                self.usage = usage_of(&json, Protocol::Responses);
                                self.terminal = true;
                            }
                            Some("response.failed" | "error") => {
                                self.terminal = true;
                                self.errored = true;
                            }
                            _ => {}
                        }
                    }
                    _ => {
                        if json.get("error").is_some_and(|e| !e.is_null()) {
                            self.terminal = true;
                            self.errored = true;
                        }
                        if let Some(m) = json.get_mut("model") {
                            *m = Json::String(self.client_model.clone());
                        }
                        let is_usage_chunk = json.get("usage").is_some_and(Json::is_object)
                            && json
                                .get("choices")
                                .and_then(Json::as_array)
                                .is_some_and(Vec::is_empty);
                        if let Some(u) = json.get("usage").filter(|u| u.is_object()) {
                            self.usage = super::chat_parse::usage(u);
                        }
                        if is_usage_chunk && self.hide_usage_chunk {
                            continue;
                        }
                    }
                }
                data = json.to_string();
            }
            out.extend(format_event(name.as_deref(), &data));
        }
        out
    }

    fn error_event(&mut self, message: &str) -> Vec<u8> {
        self.errored = true;
        match self.protocol {
            Protocol::Responses => {
                let a = format_event(Some("error"), &json!({"type": "error", "code": "stream_truncated", "message": message}).to_string());
                let b = format_event(Some("response.failed"), &json!({"type": "response.failed", "response": {"status": "failed", "error": {"code": "stream_truncated", "message": message}}}).to_string());
                [a, b].concat()
            }
            _ => format_event(None, &json!({"error": {"message": message, "type": "server_error", "code": "stream_truncated"}}).to_string()),
        }
    }

    /// Feeds upstream bytes; returns the bytes to send the client.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<u8> {
        match self.sse.push(bytes) {
            Ok(events) => self.handle(events),
            Err(e) => self.error_event(&e.to_string()),
        }
    }

    /// Ends the stream, with an error event if it was cut off.
    pub fn finish(&mut self) -> Vec<u8> {
        let mut out = match self.sse.finish() {
            Ok(events) => self.handle(events),
            Err(e) => self.error_event(&e.to_string()),
        };
        if !self.terminal && !self.errored {
            out.extend(self.error_event(&StreamError::truncated().message));
        }
        out
    }

    /// Ends the stream because the upstream broke or went silent.
    pub fn fail(&mut self, message: &str) -> Vec<u8> {
        if self.errored || self.terminal {
            return Vec::new();
        }
        self.error_event(message)
    }

    /// Bytes to send while the upstream is silent.
    #[must_use]
    pub fn keepalive() -> Vec<u8> {
        crate::sse::format_comment("keep-alive")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flavor::Flavor;

    fn target() -> Target {
        Target::new("up", Protocol::Chat, Flavor::OpenAi, "gpt-upstream")
    }

    #[test]
    fn the_model_is_mapped_and_everything_else_forwarded() {
        let body = br#"{"model":"gpt-4.1","messages":[],"logprobs":true,"extra":{"a":1}}"#;
        let (req, _) = build(body, &target(), Protocol::Chat).unwrap();
        let sent: Json = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(sent["model"], "gpt-upstream");
        assert_eq!(
            sent["logprobs"], true,
            "a passthrough does not judge fields the upstream may take"
        );
        assert_eq!(sent["extra"], json!({"a": 1}));
        assert_eq!(req.path, "/v1/chat/completions");
        assert!(!req.stream);
        let (req, _) = build(
            br#"{"model":"m","input":"x"}"#,
            &target(),
            Protocol::Responses,
        )
        .unwrap();
        assert_eq!(req.path, "/v1/responses");
        assert!(build(b"[]", &target(), Protocol::Chat).is_err());
    }

    #[test]
    fn chat_streams_always_ask_for_usage_and_remember_whether_the_client_did() {
        let (req, wants) =
            build(br#"{"model":"m","stream":true}"#, &target(), Protocol::Chat).unwrap();
        assert!(!wants);
        assert_eq!(
            serde_json::from_slice::<Json>(&req.body).unwrap()["stream_options"]["include_usage"],
            true
        );
        let (_, wants) = build(
            br#"{"model":"m","stream":true,"stream_options":{"include_usage":true}}"#,
            &target(),
            Protocol::Chat,
        )
        .unwrap();
        assert!(wants);
        let (req, _) = build(
            br#"{"model":"m","stream":false}"#,
            &target(),
            Protocol::Chat,
        )
        .unwrap();
        assert!(serde_json::from_slice::<Json>(&req.body)
            .unwrap()
            .get("stream_options")
            .is_none());
    }

    #[test]
    fn replies_echo_the_clients_model_and_expose_usage() {
        let reply = br#"{"id":"c","model":"gpt-upstream","choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"prompt_tokens_details":{"cached_tokens":4}}}"#;
        let (out, usage) = rewrite_response(reply, "gpt-4.1", Protocol::Chat);
        assert_eq!(
            serde_json::from_slice::<Json>(&out).unwrap()["model"],
            "gpt-4.1"
        );
        assert_eq!(
            (
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens
            ),
            (6, 3, 4)
        );
        let reply = br#"{"id":"r","model":"gpt-upstream","output":[],"usage":{"input_tokens":8,"output_tokens":2}}"#;
        let (out, usage) = rewrite_response(reply, "gpt-5", Protocol::Responses);
        assert_eq!(
            serde_json::from_slice::<Json>(&out).unwrap()["model"],
            "gpt-5"
        );
        assert_eq!((usage.input_tokens, usage.output_tokens), (8, 2));
        assert_eq!(
            rewrite_response(b"not json", "m", Protocol::Chat).0,
            b"not json"
        );
    }

    fn chat_wire() -> String {
        [
            r#"data: {"id":"c","model":"gpt-upstream","choices":[{"index":0,"delta":{"content":"hi"},"finish_reason":null}]}"#,
            r#"data: {"id":"c","model":"gpt-upstream","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            r#"data: {"id":"c","model":"gpt-upstream","choices":[],"usage":{"prompt_tokens":5,"completion_tokens":2}}"#,
            "data: [DONE]",
        ]
        .join("\n\n")
            + "\n\n"
    }

    fn run(
        mut s: JsonPassthroughStream,
        wire: &str,
        size: usize,
    ) -> (String, JsonPassthroughStream) {
        let mut out = Vec::new();
        for c in wire.as_bytes().chunks(size) {
            out.extend(s.push(c));
        }
        out.extend(s.finish());
        (String::from_utf8(out).unwrap(), s)
    }

    #[test]
    fn chat_chunks_are_rewritten_and_the_usage_chunk_is_kept_when_asked_for() {
        for size in [1000, 1, 7] {
            let (out, s) = run(
                JsonPassthroughStream::new(Protocol::Chat, "gpt-4.1", true),
                &chat_wire(),
                size,
            );
            assert!(
                out.contains("\"model\":\"gpt-4.1\"") && !out.contains("gpt-upstream"),
                "{size}"
            );
            assert!(out.contains("\"usage\"") && out.contains("data: [DONE]"));
            assert!(s.completed());
            assert_eq!((s.usage().input_tokens, s.usage().output_tokens), (5, 2));
        }
    }

    #[test]
    fn a_usage_chunk_the_client_did_not_ask_for_is_hidden_but_still_counted() {
        let (out, s) = run(
            JsonPassthroughStream::new(Protocol::Chat, "gpt-4.1", false),
            &chat_wire(),
            1000,
        );
        assert!(!out.contains("\"usage\""), "{out}");
        assert!(out.contains("[DONE]"));
        assert_eq!(s.usage().output_tokens, 2, "it is still accounted for");
    }

    #[test]
    fn a_truncated_chat_stream_ends_with_an_error_object() {
        let wire = chat_wire();
        let cut = &wire[..wire.find("data: [DONE]").unwrap()];
        let (out, s) = run(
            JsonPassthroughStream::new(Protocol::Chat, "m", true),
            cut,
            1000,
        );
        assert!(out.contains("stream_truncated") && !out.contains("[DONE]"));
        assert!(!s.completed());
        let (out, _) = run(
            JsonPassthroughStream::new(Protocol::Chat, "m", true),
            "",
            10,
        );
        assert!(out.contains("stream_truncated"));
    }

    #[test]
    fn responses_events_are_rewritten_and_the_terminal_event_is_required() {
        let wire = "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{\"id\":\"r\",\"model\":\"gpt-upstream\"}}\n\nevent: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r\",\"model\":\"gpt-upstream\",\"usage\":{\"input_tokens\":9,\"output_tokens\":4}}}\n\n";
        let (out, s) = run(
            JsonPassthroughStream::new(Protocol::Responses, "gpt-5", true),
            wire,
            13,
        );
        assert!(out.contains("\"model\":\"gpt-5\"") && !out.contains("gpt-upstream"));
        assert!(s.completed());
        assert_eq!((s.usage().input_tokens, s.usage().output_tokens), (9, 4));
        let cut = &wire[..wire.find("event: response.completed").unwrap()];
        let (out, s) = run(
            JsonPassthroughStream::new(Protocol::Responses, "gpt-5", true),
            cut,
            1000,
        );
        assert!(out.contains("event: error") && out.contains("event: response.failed"));
        assert!(!s.completed());
    }

    #[test]
    fn upstream_error_events_are_forwarded_once() {
        let (out, s) = run(
            JsonPassthroughStream::new(Protocol::Chat, "m", true),
            "data: {\"error\":{\"message\":\"boom\"}}\n\n",
            1000,
        );
        assert_eq!(out.matches("\"error\"").count(), 1);
        assert!(!s.completed());
        let (out, _) = run(
            JsonPassthroughStream::new(Protocol::Responses, "m", true),
            "event: error\ndata: {\"type\":\"error\",\"message\":\"x\"}\n\n",
            1000,
        );
        assert_eq!(out.matches("event: error").count(), 1);
    }

    #[test]
    fn fail_and_keepalive_and_oversized_input() {
        let mut s = JsonPassthroughStream::new(Protocol::Chat, "m", true);
        assert!(String::from_utf8(s.fail("broke"))
            .unwrap()
            .contains("broke"));
        assert!(s.fail("again").is_empty());
        assert_eq!(JsonPassthroughStream::keepalive(), b": keep-alive\n\n");
        let mut s = JsonPassthroughStream::new(Protocol::Chat, "m", true);
        s.sse = SseParser::with_limits(8, 8);
        assert!(String::from_utf8(s.push(b"data: 0123456789abcdef\n\n"))
            .unwrap()
            .contains("error"));
        let mut s = JsonPassthroughStream::new(Protocol::Chat, "m", true);
        s.sse = SseParser::with_limits(16, 8);
        let _ = s.push(b"data: 12345\ndata: 678");
        assert!(String::from_utf8(s.finish()).unwrap().contains("error"));
    }

    #[test]
    fn non_json_events_pass_through_verbatim() {
        let (out, _) = run(
            JsonPassthroughStream::new(Protocol::Chat, "m", true),
            "data: hello\n\ndata: [DONE]\n\n",
            1000,
        );
        assert!(out.starts_with("data: hello\n\n"));
    }
}
