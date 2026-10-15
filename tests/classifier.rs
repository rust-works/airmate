//! The HTTP classifier against a scripted endpoint.

mod support;

use std::sync::Arc;

use airmate::config::schema::{ClassifierConfig, ClassifierInputField};
use airmate::router::classifier::HttpClassifier;
use airmate::router::rules::{Classifier, ClassifierInput};
use serde_json::json;
use support::{Capture, Mock, Reply};

fn config(url: &str, timeout_ms: u64) -> ClassifierConfig {
    ClassifierConfig {
        name: "complexity".into(),
        url: url.into(),
        timeout_ms,
        input: vec![
            ClassifierInputField::LastUserMessage,
            ClassifierInputField::Summary,
        ],
    }
}

fn input() -> ClassifierInput {
    ClassifierInput {
        last_user_message: Some("fix the bug".into()),
        system: Some("TOP SECRET SYSTEM".into()),
        summary: Some("messages=1 tools=0 est_tokens=5".into()),
    }
}

#[tokio::test]
async fn a_label_comes_back_and_only_allowed_fields_are_sent() {
    let mock = Mock::start(|_| Reply::json(200, json!({"label": "hard"}))).await;
    let cap = Arc::new(Capture::default());
    let c = HttpClassifier::new(
        &config(&format!("{}/classify", mock.url()), 2000),
        cap.clone(),
    );
    assert_eq!(c.classify(&input()).await.as_deref(), Some("hard"));
    let sent = mock.recorded();
    assert_eq!(
        (sent[0].method.as_str(), sent[0].path.as_str()),
        ("POST", "/classify")
    );
    assert_eq!(
        sent[0].json(),
        json!({"input": {"last_user_message": "fix the bug", "summary": "messages=1 tools=0 est_tokens=5"}})
    );
    assert!(
        !String::from_utf8_lossy(&sent[0].body).contains("SECRET"),
        "the system prompt was not allowed"
    );
    let recs = cap.all();
    assert_eq!((recs[0].status, recs[0].error_class.clone()), (200, None));
}

#[tokio::test]
async fn failures_slowness_and_garbage_all_fail_open() {
    let cap = Arc::new(Capture::default());
    for reply in [
        Reply::json(500, json!({"error": "boom"})),
        Reply::json(200, json!({"no_label": true})),
        Reply::json(200, json!({"label": 5})),
        Reply::Slow(std::time::Duration::from_secs(5)),
    ] {
        let mock = Mock::start(move |_| reply.clone()).await;
        let c = HttpClassifier::new(&config(&mock.url(), 300), cap.clone());
        assert_eq!(c.classify(&input()).await, None);
    }
    assert_eq!(cap.all().len(), 4, "every call is logged");
    assert!(cap
        .all()
        .iter()
        .all(|r| r.error_class.as_deref() == Some("classifier_no_label")));
}
