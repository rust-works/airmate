//! The HTTP classifier: an optional external endpoint that labels a request.
//!
//! The endpoint has a short timeout and a fail-open default: no label means the rules fall
//! through. It sees only what the classifier's `input` list allows, never the upstream key, and
//! every call is logged.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::{Method, Request};
use serde_json::{json, Map, Value as Json};

use airmate_core::facts::Facts;
use airmate_core::ir::Protocol;

use super::rules::{Classifier, ClassifierInput};
use crate::config::schema::{ClassifierConfig, ClassifierInputField};
use crate::obs::{now, RecordKind, RecordSink, RequestRecord};
use crate::server::upstream::{build_client, HttpClient};

/// A classifier reached over HTTP.
pub struct HttpClassifier {
    name: String,
    url: String,
    timeout: Duration,
    allowed: Vec<ClassifierInputField>,
    client: HttpClient,
    sink: Arc<dyn RecordSink>,
}

impl HttpClassifier {
    /// A classifier for `cfg`; calls are logged to `sink`.
    #[must_use]
    pub fn new(cfg: &ClassifierConfig, sink: Arc<dyn RecordSink>) -> Self {
        Self {
            name: cfg.name.clone(),
            url: cfg.url.clone(),
            timeout: Duration::from_millis(cfg.timeout_ms),
            allowed: cfg.input.clone(),
            client: build_client(Duration::from_millis(cfg.timeout_ms)),
            sink,
        }
    }

    /// The request body: only the fields the configuration allows.
    #[must_use]
    pub fn payload(&self, input: &ClassifierInput) -> Json {
        let mut m = Map::new();
        for f in &self.allowed {
            match f {
                ClassifierInputField::LastUserMessage => {
                    m.insert("last_user_message".into(), json!(input.last_user_message));
                }
                ClassifierInputField::System => {
                    m.insert("system".into(), json!(input.system));
                }
                ClassifierInputField::Summary => {
                    m.insert("summary".into(), json!(input.summary));
                }
            }
        }
        json!({ "input": Json::Object(m) })
    }

    async fn call(&self, input: &ClassifierInput) -> (u16, Option<String>) {
        let body = serde_json::to_vec(&self.payload(input)).expect("a JSON value serialises");
        let Ok(req) = Request::builder()
            .method(Method::POST)
            .uri(&self.url)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(body)))
        else {
            return (0, None);
        };
        let work = async {
            let resp = self.client.request(req).await.ok()?;
            let status = resp.status().as_u16();
            if !resp.status().is_success() {
                return Some((status, None));
            }
            let bytes = Limited::new(resp.into_body(), 64 * 1024)
                .collect()
                .await
                .ok()?
                .to_bytes();
            let label = serde_json::from_slice::<Json>(&bytes)
                .ok()?
                .get("label")?
                .as_str()?
                .to_owned();
            Some((status, Some(label)))
        };
        match tokio::time::timeout(self.timeout, work).await {
            Ok(Some(r)) => r,
            _ => (0, None),
        }
    }
}

impl Classifier for HttpClassifier {
    fn classify<'a>(
        &'a self,
        input: &'a ClassifierInput,
    ) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let (status, label) = self.call(input).await;
            self.sink.record(&RequestRecord {
                kind: RecordKind::Classifier,
                ts: now(),
                target: Some(format!("classifier:{}", self.name)),
                status,
                error_class: label.is_none().then(|| "classifier_no_label".to_owned()),
                total_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
                ..RequestRecord::default()
            });
            label
        })
    }
}

fn text_of(v: &Json) -> Option<String> {
    match v {
        Json::String(s) => Some(s.clone()),
        Json::Array(parts) => {
            let t: Vec<&str> = parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Json::as_str))
                .collect();
            (!t.is_empty()).then(|| t.join("\n"))
        }
        _ => None,
    }
}

/// Builds what a classifier may see from the raw request body.
#[must_use]
pub fn input_from_body(inbound: Protocol, body: &[u8], facts: &Facts) -> ClassifierInput {
    let Ok(v) = serde_json::from_slice::<Json>(body) else {
        return ClassifierInput::default();
    };
    let last_of = |items: Option<&Vec<Json>>| -> Option<String> {
        items?
            .iter()
            .rev()
            .find(|m| m.get("role").and_then(Json::as_str) == Some("user"))
            .and_then(|m| m.get("content").and_then(text_of))
    };
    let (last_user_message, system, count) = match inbound {
        Protocol::Anthropic => (
            last_of(v.get("messages").and_then(Json::as_array)),
            v.get("system").and_then(text_of),
            v.get("messages")
                .and_then(Json::as_array)
                .map_or(0, Vec::len),
        ),
        Protocol::Chat => {
            let msgs = v.get("messages").and_then(Json::as_array);
            let system = msgs.and_then(|m| {
                m.iter()
                    .find(|x| {
                        matches!(
                            x.get("role").and_then(Json::as_str),
                            Some("system" | "developer")
                        )
                    })
                    .and_then(|x| x.get("content").and_then(text_of))
            });
            (last_of(msgs), system, msgs.map_or(0, Vec::len))
        }
        _ => {
            let input = v.get("input");
            let last = match input {
                Some(Json::String(s)) => Some(s.clone()),
                Some(Json::Array(a)) => last_of(Some(a)),
                _ => None,
            };
            (
                last,
                v.get("instructions")
                    .and_then(Json::as_str)
                    .map(str::to_owned),
                input.and_then(Json::as_array).map_or(1, Vec::len),
            )
        }
    };
    ClassifierInput {
        last_user_message,
        system,
        summary: Some(format!(
            "messages={count} tools={} est_tokens={}",
            v.get("tools").and_then(Json::as_array).map_or(0, Vec::len),
            facts.est_input_tokens
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::NullSink;

    fn facts() -> Facts {
        Facts {
            inbound: Protocol::Anthropic,
            model: "m".into(),
            stream: false,
            tools: true,
            images: false,
            thinking: false,
            structured_output: false,
            effort: None,
            est_input_tokens: 123,
            max_tokens: 10,
        }
    }

    #[test]
    fn anthropic_bodies_yield_the_last_user_message_system_and_a_summary() {
        let body = br#"{"system":[{"type":"text","text":"be brief"}],"tools":[{"name":"t"}],"messages":[{"role":"user","content":"first"},{"role":"assistant","content":"a"},{"role":"user","content":[{"type":"text","text":"second"},{"type":"text","text":"line"}]}]}"#;
        let i = input_from_body(Protocol::Anthropic, body, &facts());
        assert_eq!(i.last_user_message.as_deref(), Some("second\nline"));
        assert_eq!(i.system.as_deref(), Some("be brief"));
        assert_eq!(
            i.summary.as_deref(),
            Some("messages=3 tools=1 est_tokens=123")
        );
    }

    #[test]
    fn chat_and_responses_bodies_too() {
        let chat = br#"{"messages":[{"role":"system","content":"sys"},{"role":"user","content":"hello"}]}"#;
        let i = input_from_body(Protocol::Chat, chat, &facts());
        assert_eq!(
            (i.last_user_message.as_deref(), i.system.as_deref()),
            (Some("hello"), Some("sys"))
        );
        let resp = br#"{"instructions":"rules","input":[{"role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        let i = input_from_body(Protocol::Responses, resp, &facts());
        assert_eq!(
            (i.last_user_message.as_deref(), i.system.as_deref()),
            (Some("hi"), Some("rules"))
        );
        let i = input_from_body(Protocol::Responses, br#"{"input":"plain"}"#, &facts());
        assert_eq!(i.last_user_message.as_deref(), Some("plain"));
        assert_eq!(
            input_from_body(Protocol::Chat, b"not json", &facts()),
            ClassifierInput::default()
        );
    }

    #[test]
    fn the_payload_contains_only_the_allowed_fields() {
        let cfg = ClassifierConfig {
            name: "c".into(),
            url: "http://127.0.0.1:1".into(),
            timeout_ms: 50,
            input: vec![ClassifierInputField::Summary],
        };
        let c = HttpClassifier::new(&cfg, Arc::new(NullSink));
        let input = ClassifierInput {
            last_user_message: Some("SECRET PROMPT".into()),
            system: Some("SECRET SYSTEM".into()),
            summary: Some("s".into()),
        };
        let p = c.payload(&input);
        assert_eq!(p, json!({"input": {"summary": "s"}}));
        assert!(!p.to_string().contains("SECRET"));
        let all = ClassifierConfig {
            input: vec![
                ClassifierInputField::LastUserMessage,
                ClassifierInputField::System,
                ClassifierInputField::Summary,
            ],
            ..cfg
        };
        assert_eq!(
            HttpClassifier::new(&all, Arc::new(NullSink)).payload(&input)["input"]
                .as_object()
                .unwrap()
                .len(),
            3
        );
        assert_eq!(
            c.payload(&ClassifierInput::default()),
            json!({"input": {"summary": null}})
        );
    }

    #[tokio::test]
    async fn an_unreachable_classifier_gives_no_label_and_is_logged() {
        #[derive(Default)]
        struct Capture(std::sync::Mutex<Vec<RequestRecord>>);
        impl RecordSink for Capture {
            fn record(&self, r: &RequestRecord) {
                self.0.lock().unwrap().push(r.clone());
            }
        }
        let cap = Arc::new(Capture::default());
        let cfg = ClassifierConfig {
            name: "c".into(),
            url: "http://127.0.0.1:1".into(),
            timeout_ms: 200,
            input: vec![],
        };
        let c = HttpClassifier::new(&cfg, cap.clone());
        assert_eq!(c.classify(&ClassifierInput::default()).await, None);
        let recs = cap.0.lock().unwrap();
        assert_eq!(recs.len(), 1);
        assert_eq!(
            (
                recs[0].kind,
                recs[0].target.as_deref(),
                recs[0].error_class.as_deref()
            ),
            (
                RecordKind::Classifier,
                Some("classifier:c"),
                Some("classifier_no_label")
            )
        );
    }
}
