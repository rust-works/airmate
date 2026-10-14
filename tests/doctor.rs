//! `airmate doctor`: the probes against scripted upstreams, library and command line.

mod support;

use std::process::Command;
use std::sync::Arc;

use airmate::config::load_str;
use airmate::doctor::{render, run_target, Outcome};
use airmate::secret::{Secret, SystemResolver};
use airmate::server::Tables;
use serde_json::{json, Value};
use support::{Mock, Part, Reply};

fn chat_stream(text: &str) -> Reply {
    let c = |d: Value, f: Value| {
        Part::Bytes(format!("data: {}\n\n", json!({"id": "c", "model": "m", "choices": [{"index": 0, "delta": d, "finish_reason": f}]})).into_bytes())
    };
    Reply::sse(vec![
        c(json!({"role": "assistant", "content": text}), Value::Null),
        c(json!({}), json!("stop")),
        Part::Bytes(b"data: [DONE]\n\n".to_vec()),
    ])
}

fn chat_tool_stream() -> Reply {
    let c = |d: Value, f: Value| {
        Part::Bytes(format!("data: {}\n\n", json!({"id": "c", "model": "m", "choices": [{"index": 0, "delta": d, "finish_reason": f}]})).into_bytes())
    };
    Reply::sse(vec![
        c(
            json!({"role": "assistant", "tool_calls": [{"index": 0, "id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": ""}}]}),
            Value::Null,
        ),
        c(
            json!({"tool_calls": [{"index": 0, "function": {"arguments": "{\"city\":\"Paris\"}"}}]}),
            Value::Null,
        ),
        c(json!({}), json!("tool_calls")),
        Part::Bytes(b"data: [DONE]\n\n".to_vec()),
    ])
}

/// An upstream that behaves: text for plain probes, a tool call for tool probes.
async fn good_upstream() -> Mock {
    Mock::start(|r| {
        let b = r.json();
        let tools = b.get("tools").is_some();
        let stream = b["stream"] == true;
        match (tools, stream) {
            (false, false) => Reply::json(200, json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 5, "completion_tokens": 1}})),
            (false, true) => chat_stream("pong"),
            (true, false) => Reply::json(200, json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": null, "tool_calls": [{"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]}, "finish_reason": "tool_calls"}]})),
            (true, true) => chat_tool_stream(),
        }
    })
    .await
}

fn config(mock: &Mock, catalog: &str) -> String {
    format!("upstreams:\n  - {{ name: up, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\n{catalog}", mock.url())
}

fn tables(yaml: &str) -> (Arc<airmate::config::Config>, Tables) {
    let cfg = Arc::new(load_str(yaml, None).unwrap());
    let (t, _) = Tables::build(
        cfg.clone(),
        &SystemResolver::new(&|_| None),
        &Secret::new("t"),
    )
    .unwrap();
    (cfg, t)
}

#[tokio::test]
async fn every_probe_passes_against_a_well_behaved_upstream() {
    let mock = good_upstream().await;
    let (_, t) = tables(&config(&mock, ""));
    let target = t.models.entries()[0].targets[0].clone();
    let report = run_target(&target, &t.catalog.for_target(&target)).await;
    let names: Vec<_> = report.checks.iter().map(|c| (c.name, c.outcome)).collect();
    assert_eq!(
        names,
        [
            ("plain call", Outcome::Pass),
            ("streamed call", Outcome::Pass),
            ("tool call", Outcome::Pass),
            ("streamed tool call", Outcome::Pass)
        ],
        "{}",
        render(std::slice::from_ref(&report))
    );
    assert!(!report.failed());
    assert_eq!(
        (
            report.upstream.as_str(),
            report.model.as_str(),
            report.protocol
        ),
        ("up", "m", "chat")
    );
}

#[tokio::test]
async fn tool_probes_are_skipped_for_a_model_the_catalog_says_cannot_call_tools() {
    let mock = good_upstream().await;
    let (_, t) = tables(&config(
        &mock,
        "catalog:\n  - { match: \"m\", tools: false }\n",
    ));
    let target = t.models.entries()[0].targets[0].clone();
    let report = run_target(&target, &t.catalog.for_target(&target)).await;
    assert_eq!(report.checks[2].outcome, Outcome::Skip);
    assert_eq!(report.checks[3].outcome, Outcome::Skip);
    assert!(!report.failed());
    assert_eq!(mock.count(), 2, "only the plain probes were sent");
}

#[tokio::test]
async fn a_model_that_answers_a_tool_probe_with_text_fails_it() {
    let mock = Mock::start(|r| {
        if r.json()["stream"] == true {
            chat_stream("sunny")
        } else {
            Reply::json(200, json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "sunny"}, "finish_reason": "stop"}]}))
        }
    })
    .await;
    let (_, t) = tables(&config(&mock, ""));
    let target = t.models.entries()[0].targets[0].clone();
    let report = run_target(&target, &t.catalog.for_target(&target)).await;
    assert_eq!(report.checks[0].outcome, Outcome::Pass);
    assert_eq!(report.checks[2].outcome, Outcome::Fail);
    assert!(
        report.checks[2].detail.contains("did not call the tool"),
        "{}",
        report.checks[2].detail
    );
    assert!(report.failed());
}

#[tokio::test]
async fn an_upstream_error_is_reported_with_its_status_and_message() {
    let mock = Mock::start(|_| {
        Reply::json(
            401,
            json!({"error": {"message": "Incorrect API key provided"}}),
        )
    })
    .await;
    let (_, t) = tables(&config(&mock, ""));
    let target = t.models.entries()[0].targets[0].clone();
    let report = run_target(&target, &t.catalog.for_target(&target)).await;
    assert!(
        report
            .checks
            .iter()
            .filter(|c| c.outcome != Outcome::Skip)
            .all(|c| c.outcome == Outcome::Fail
                && c.detail.contains("HTTP 401")
                && c.detail.contains("Incorrect API key")),
        "{}",
        render(std::slice::from_ref(&report))
    );
}

#[tokio::test]
async fn a_stream_that_breaks_before_its_end_fails_the_probe() {
    let mock = Mock::start(|r| {
        if r.json()["stream"] == true {
            Reply::sse(vec![Part::Bytes(b"data: {\"id\":\"c\",\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"pa\"},\"finish_reason\":null}]}\n\n".to_vec()), Part::Reset])
        } else {
            Reply::json(200, json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "pong"}, "finish_reason": "stop"}]}))
        }
    })
    .await;
    let (_, t) = tables(&config(
        &mock,
        "catalog:\n  - { match: \"m\", tools: false }\n",
    ));
    let target = t.models.entries()[0].targets[0].clone();
    let report = run_target(&target, &t.catalog.for_target(&target)).await;
    assert_eq!(report.checks[0].outcome, Outcome::Pass);
    assert_eq!(
        report.checks[1].outcome,
        Outcome::Fail,
        "{}",
        report.checks[1].detail
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_command_exits_0_when_everything_passes_and_3_when_a_probe_fails() {
    let dir = std::path::PathBuf::from(format!("/tmp/am-doctor-{}", std::process::id()));
    let cfg_dir = dir.join("config/airmate");
    std::fs::create_dir_all(&cfg_dir).unwrap();
    let run = |mock: &Mock, args: &[&str]| {
        std::fs::write(cfg_dir.join("airmate.yaml"), config(mock, "")).unwrap();
        Command::new(env!("CARGO_BIN_EXE_airmate"))
            .env_clear()
            .env("HOME", &dir)
            .env("XDG_CONFIG_HOME", dir.join("config"))
            .env("XDG_STATE_HOME", dir.join("state"))
            .arg("doctor")
            .args(args)
            .output()
            .unwrap()
    };
    let good = good_upstream().await;
    let o = run(&good, &[]);
    let text = String::from_utf8_lossy(&o.stdout).into_owned();
    assert!(
        o.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&o.stderr)
    );
    assert!(
        text.contains("up / m (chat)")
            && text.contains("ok   plain call")
            && text.contains("ok   streamed tool call"),
        "{text}"
    );

    let o = run(&good, &["--json"]);
    let v: Value = serde_json::from_slice(&o.stdout).unwrap();
    assert_eq!(v[0]["checks"].as_array().unwrap().len(), 4);
    assert_eq!(v[0]["checks"][0]["outcome"], "pass");

    let bad = Mock::start(|_| Reply::json(500, json!({"error": {"message": "down"}}))).await;
    let o = run(&bad, &[]);
    assert_eq!(
        o.status.code(),
        Some(3),
        "{}",
        String::from_utf8_lossy(&o.stdout)
    );
    assert!(String::from_utf8_lossy(&o.stdout).contains("FAIL plain call"));

    let o = run(&good, &["--upstream", "ghost"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("`ghost` is not a configured upstream"));
    let _ = std::fs::remove_dir_all(&dir);
}
