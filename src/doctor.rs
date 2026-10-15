//! `airmate doctor`: a live self-test of one upstream model.
//!
//! The probes go straight to the upstream through the same adapters the gateway uses: build the
//! request from the IR, send it, parse the reply. A pass therefore shows that the translation
//! works against that endpoint, not just that the endpoint is up. The same engine drives the live
//! conformance tests, which are gated on environment variables and never run in the default CI.
//!
//! The probes are: a plain call, a streamed call, and (for models that take tools) a call that
//! must use a tool, plain and streamed.

use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use serde::Serialize;
use serde_json::json;

use airmate_core::adapters;
use airmate_core::ir::{
    FinishReason, IrCompletion, IrRequest, Part, Protocol, ToolChoice, ToolChoiceKind, ToolDef,
    Turn,
};
use airmate_core::outbound::{ParseCtx, UpstreamError, UpstreamRequest};
use airmate_core::stream::IrEvent;

use crate::router::catalog::Capabilities;
use crate::router::RouteTarget;
use crate::server::upstream::send;

/// How a probe ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    /// It worked.
    Pass,
    /// It did not.
    Fail,
    /// It was not attempted, with the reason in the detail.
    Skip,
}

/// One probe's result.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Check {
    /// The probe.
    pub name: &'static str,
    /// How it ended.
    pub outcome: Outcome,
    /// How long it took.
    pub ms: u64,
    /// What happened.
    pub detail: String,
}

/// The results for one upstream model.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    /// The upstream.
    pub upstream: String,
    /// The model sent upstream.
    pub model: String,
    /// The protocol the upstream speaks.
    pub protocol: &'static str,
    /// The probes.
    pub checks: Vec<Check>,
}

impl Report {
    /// Whether any probe failed.
    #[must_use]
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.outcome == Outcome::Fail)
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_secs(90);

fn base_request(stream: bool) -> IrRequest {
    let mut ir = IrRequest::new("doctor", Protocol::Anthropic);
    ir.max_output_tokens = 256;
    ir.stream = stream;
    ir.turns.push(Turn::User(vec![Part::Text(
        "Reply with the single word: pong".to_owned(),
    )]));
    ir
}

fn tool_request(stream: bool) -> IrRequest {
    let mut ir = IrRequest::new("doctor", Protocol::Anthropic);
    ir.max_output_tokens = 256;
    ir.stream = stream;
    ir.turns.push(Turn::User(vec![Part::Text(
        "What is the weather in Paris? Use the tool.".to_owned(),
    )]));
    ir.tools.push(ToolDef {
        name: "get_weather".to_owned(),
        description: Some("Gets the current weather for a city.".to_owned()),
        input_schema: json!({"type": "object", "properties": {"city": {"type": "string"}}, "required": ["city"]}),
        strict: None,
    });
    ir.tool_choice = ToolChoice {
        kind: ToolChoiceKind::Required,
        parallel: true,
    };
    ir
}

fn has_text(parts: &[Part]) -> bool {
    parts
        .iter()
        .any(|p| matches!(p, Part::Text(t) if !t.trim().is_empty()))
}

fn tool_call_ok(parts: &[Part]) -> Result<(), String> {
    let call = parts.iter().find_map(|p| match p {
        Part::ToolCall { name, input, .. } => Some((name, input)),
        _ => None,
    });
    match call {
        None => Err("the model did not call the tool".to_owned()),
        Some((name, _)) if name != "get_weather" => {
            Err(format!("the model called `{name}`, not get_weather"))
        }
        Some((_, input)) if !input.get("city").is_some_and(serde_json::Value::is_string) => {
            Err(format!("the tool arguments have no `city`: {input}"))
        }
        Some(_) => Ok(()),
    }
}

async fn call(
    target: &RouteTarget,
    ir: &IrRequest,
    text_wanted: bool,
    tool_wanted: bool,
) -> Result<String, String> {
    let core = target.core_target(true);
    let request: UpstreamRequest =
        adapters::build_request(ir, &core).map_err(|e| format!("cannot build the request: {e}"))?;
    let ctx = ParseCtx::new(&core, &request);
    let resp = send(&target.upstream, &request, &hyper::HeaderMap::new())
        .await
        .map_err(|e| e.into_gateway(&target.upstream.name).message)?;
    let status = resp.status.as_u16();
    if !resp.status.is_success() {
        let body = resp
            .body
            .collect()
            .await
            .map(|c| c.to_bytes())
            .unwrap_or_default();
        let e = UpstreamError::from_response(status, &body);
        return Err(format!("HTTP {status}: {}", e.message));
    }
    let (content, finish): (Vec<Part>, FinishReason) = if request.stream {
        let mut parser = adapters::stream_parser(core.protocol, ctx);
        let mut events: Vec<IrEvent> = Vec::new();
        let mut body = resp.body;
        while let Some(frame) = body.frame().await {
            match frame {
                Ok(f) => {
                    if let Ok(data) = f.into_data() {
                        events.extend(parser.push(&data));
                    }
                }
                Err(e) => return Err(format!("the stream broke: {e}")),
            }
        }
        events.extend(parser.finish());
        fold(&events)?
    } else {
        let body = resp
            .body
            .collect()
            .await
            .map_err(|e| e.to_string())?
            .to_bytes();
        let c: IrCompletion = adapters::parse_response(core.protocol, status, &body, &ctx)
            .map_err(|e| format!("unreadable reply: {}", e.message))?;
        (c.content, c.finish)
    };
    if text_wanted && !has_text(&content) {
        return Err("the reply has no text".to_owned());
    }
    if tool_wanted {
        tool_call_ok(&content)?;
        if finish != FinishReason::ToolCalls {
            return Err(format!("the finish reason is {finish:?}, not a tool call"));
        }
    }
    Ok(format!("finished with {finish:?}"))
}

/// Folds stream events into content and a finish reason; an error event is a failed probe.
fn fold(events: &[IrEvent]) -> Result<(Vec<Part>, FinishReason), String> {
    let mut text = String::new();
    let mut calls: std::collections::BTreeMap<u32, (String, String, String)> =
        std::collections::BTreeMap::new();
    let mut finish = None;
    for e in events {
        match e {
            IrEvent::TextDelta { text: t, .. } => text.push_str(t),
            IrEvent::ToolCallStart { block, id, name } => {
                calls.insert(*block, (id.clone(), name.clone(), String::new()));
            }
            IrEvent::ToolCallArgs { block, fragment } => {
                if let Some(c) = calls.get_mut(block) {
                    c.2.push_str(fragment);
                }
            }
            IrEvent::Finish(f) => finish = Some(*f),
            IrEvent::Error(e) => return Err(format!("stream error: {}", e.message)),
            _ => {}
        }
    }
    let finish = finish.ok_or("the stream ended without a finish")?;
    let mut parts = Vec::new();
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    for (_, (id, name, args)) in calls {
        let input = if args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&args).map_err(|e| format!("tool arguments are not JSON: {e}"))?
        };
        parts.push(Part::ToolCall { id, name, input });
    }
    Ok((parts, finish))
}

async fn probe(
    target: &RouteTarget,
    name: &'static str,
    ir: &IrRequest,
    text_wanted: bool,
    tool_wanted: bool,
) -> Check {
    let start = Instant::now();
    let result =
        tokio::time::timeout(PROBE_TIMEOUT, call(target, ir, text_wanted, tool_wanted)).await;
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    match result {
        Ok(Ok(detail)) => Check {
            name,
            outcome: Outcome::Pass,
            ms,
            detail,
        },
        Ok(Err(detail)) => Check {
            name,
            outcome: Outcome::Fail,
            ms,
            detail,
        },
        Err(_) => Check {
            name,
            outcome: Outcome::Fail,
            ms,
            detail: format!("no answer within {}s", PROBE_TIMEOUT.as_secs()),
        },
    }
}

/// Runs every probe against `target`. `caps` decides which are worth trying.
pub async fn run_target(target: &RouteTarget, caps: &Capabilities) -> Report {
    let mut checks = vec![
        probe(target, "plain call", &base_request(false), true, false).await,
        probe(target, "streamed call", &base_request(true), true, false).await,
    ];
    if caps.tools {
        checks.push(probe(target, "tool call", &tool_request(false), false, true).await);
        checks.push(
            probe(
                target,
                "streamed tool call",
                &tool_request(true),
                false,
                true,
            )
            .await,
        );
    } else {
        for name in ["tool call", "streamed tool call"] {
            checks.push(Check {
                name,
                outcome: Outcome::Skip,
                ms: 0,
                detail: "the catalog says this model does not call tools".to_owned(),
            });
        }
    }
    Report {
        upstream: target.upstream.name.clone(),
        model: target.model.clone(),
        protocol: target.upstream.protocol.as_str(),
        checks,
    }
}

/// Renders reports for people.
#[must_use]
pub fn render(reports: &[Report]) -> String {
    let mut out = String::new();
    for r in reports {
        out.push_str(&format!("{} / {} ({})\n", r.upstream, r.model, r.protocol));
        for c in &r.checks {
            let mark = match c.outcome {
                Outcome::Pass => "ok  ",
                Outcome::Fail => "FAIL",
                Outcome::Skip => "skip",
            };
            out.push_str(&format!(
                "  {mark} {:<20} {:>6}ms  {}\n",
                c.name, c.ms, c.detail
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use airmate_core::stream::{StreamError, StreamErrorKind};

    #[test]
    fn a_tool_call_must_name_the_tool_and_carry_a_city() {
        let call = |name: &str, input| {
            vec![Part::ToolCall {
                id: "c".into(),
                name: name.into(),
                input,
            }]
        };
        assert!(tool_call_ok(&call("get_weather", json!({"city": "Paris"}))).is_ok());
        assert!(tool_call_ok(&call("other", json!({"city": "Paris"})))
            .unwrap_err()
            .contains("not get_weather"));
        assert!(tool_call_ok(&call("get_weather", json!({})))
            .unwrap_err()
            .contains("no `city`"));
        assert!(tool_call_ok(&[Part::Text("sunny".into())])
            .unwrap_err()
            .contains("did not call"));
    }

    #[test]
    fn stream_events_fold_into_content() {
        let (parts, finish) = fold(&[
            IrEvent::Start {
                id: None,
                model: None,
            },
            IrEvent::TextDelta {
                block: 0,
                text: "po".into(),
            },
            IrEvent::TextDelta {
                block: 0,
                text: "ng".into(),
            },
            IrEvent::ToolCallStart {
                block: 1,
                id: "c".into(),
                name: "t".into(),
            },
            IrEvent::ToolCallArgs {
                block: 1,
                fragment: "{\"a\":".into(),
            },
            IrEvent::ToolCallArgs {
                block: 1,
                fragment: "1}".into(),
            },
            IrEvent::Finish(FinishReason::ToolCalls),
        ])
        .unwrap();
        assert_eq!(finish, FinishReason::ToolCalls);
        assert_eq!(
            parts,
            vec![
                Part::Text("pong".into()),
                Part::ToolCall {
                    id: "c".into(),
                    name: "t".into(),
                    input: json!({"a": 1})
                }
            ]
        );
    }

    #[test]
    fn a_stream_error_or_a_missing_finish_fails_the_probe() {
        let e = fold(&[IrEvent::Error(StreamError {
            kind: StreamErrorKind::Upstream,
            message: "boom".into(),
        })])
        .unwrap_err();
        assert!(e.contains("boom"));
        assert!(fold(&[IrEvent::TextDelta {
            block: 0,
            text: "x".into()
        }])
        .unwrap_err()
        .contains("without a finish"));
        assert!(fold(&[
            IrEvent::ToolCallStart {
                block: 0,
                id: "c".into(),
                name: "t".into()
            },
            IrEvent::ToolCallArgs {
                block: 0,
                fragment: "{".into()
            },
            IrEvent::Finish(FinishReason::ToolCalls)
        ])
        .unwrap_err()
        .contains("not JSON"));
    }

    #[test]
    fn reports_say_what_failed() {
        let r = Report {
            upstream: "up".into(),
            model: "m".into(),
            protocol: "chat",
            checks: vec![
                Check {
                    name: "plain call",
                    outcome: Outcome::Pass,
                    ms: 12,
                    detail: "finished with Stop".into(),
                },
                Check {
                    name: "tool call",
                    outcome: Outcome::Fail,
                    ms: 30,
                    detail: "HTTP 400: nope".into(),
                },
                Check {
                    name: "streamed tool call",
                    outcome: Outcome::Skip,
                    ms: 0,
                    detail: "skipped".into(),
                },
            ],
        };
        assert!(r.failed());
        let t = render(std::slice::from_ref(&r));
        assert!(
            t.starts_with("up / m (chat)\n")
                && t.contains("ok   plain call")
                && t.contains("FAIL tool call")
                && t.contains("skip streamed tool call"),
            "{t}"
        );
        let mut ok = r;
        ok.checks.remove(1);
        assert!(!ok.failed(), "a skip is not a failure");
        let json = serde_json::to_value(&ok).unwrap();
        assert_eq!(json["checks"][0]["outcome"], "pass");
    }
}
