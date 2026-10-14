//! Runs the built binary, so `src/main.rs` and the command line are exercised and covered.

mod support;

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use support::{Mock, Reply};

struct Home(PathBuf);

impl Home {
    fn new(name: &str) -> Self {
        let d = std::env::temp_dir().join(format!("airmate-cli-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        Self(d)
    }

    fn cmd(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_airmate"));
        c.env_clear()
            .env("HOME", &self.0)
            .env("XDG_CONFIG_HOME", self.0.join("config"))
            .env("XDG_STATE_HOME", self.0.join("state"))
            .env("PATH", std::env::var("PATH").unwrap_or_default());
        c
    }

    fn write_config(&self, text: &str) -> PathBuf {
        let p = self.0.join("config/airmate/airmate.yaml");
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, text).unwrap();
        p
    }

    fn token_file(&self) -> PathBuf {
        self.0.join("state/airmate/gateway.token")
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn errs(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn version_prints_the_crate_version() {
    let h = Home::new("version");
    let o = h.cmd().arg("version").output().unwrap();
    assert!(o.status.success());
    assert_eq!(out(&o), format!("airmate {}\n", env!("CARGO_PKG_VERSION")));
    let o = h.cmd().arg("--version").output().unwrap();
    assert!(out(&o).starts_with("airmate "));
}

#[test]
fn no_arguments_prints_usage_and_exits_2() {
    let h = Home::new("noargs");
    let o = h.cmd().output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(errs(&o).contains("Usage"), "{}", errs(&o));
    let o = h.cmd().arg("bogus").output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    let o = h.cmd().arg("--help").output().unwrap();
    assert!(o.status.success());
    assert!(out(&o).contains("serve") && out(&o).contains("env"));
}

#[test]
fn fields_prints_the_policy_table_as_markdown_or_json() {
    let h = Home::new("fields");
    let o = h.cmd().arg("fields").output().unwrap();
    assert!(o.status.success(), "{}", errs(&o));
    let md = out(&o);
    assert!(
        md.contains("#### Request fields") && md.contains("| `safeguards` | rejected |"),
        "{md}"
    );
    let o = h
        .cmd()
        .args(["fields", "--format", "json"])
        .output()
        .unwrap();
    let rows: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert!(rows
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["name"] == "safeguards" && r["class"] == "rejected"));
}

#[test]
fn env_creates_the_token_once_and_prints_the_default_port() {
    let h = Home::new("env");
    let o = h.cmd().arg("env").output().unwrap();
    assert!(o.status.success(), "{}", errs(&o));
    let token = std::fs::read_to_string(h.token_file()).unwrap();
    let text = out(&o);
    assert!(
        text.contains("export ANTHROPIC_BASE_URL=http://127.0.0.1:9997\n"),
        "{text}"
    );
    assert!(
        text.contains(&format!("export ANTHROPIC_AUTH_TOKEN={token}\n")),
        "{text}"
    );
    assert_eq!(
        out(&h.cmd().arg("env").output().unwrap()),
        text,
        "the same token on the next run"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(h.token_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn env_formats_and_client_families() {
    let h = Home::new("env-formats");
    let j = h.cmd().args(["env", "--format", "json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_str(&out(&j)).unwrap();
    assert_eq!(v["env"]["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9997");
    let d = out(&h
        .cmd()
        .args(["env", "--format", "dotenv"])
        .output()
        .unwrap());
    assert!(d.contains("ANTHROPIC_BASE_URL=http://127.0.0.1:9997\n") && !d.contains("export"));
    let o = out(&h.cmd().args(["env", "--inbound", "chat"]).output().unwrap());
    assert!(
        o.contains("OPENAI_BASE_URL=http://127.0.0.1:9997/v1") && o.contains("OPENAI_API_KEY="),
        "{o}"
    );
}

#[test]
fn the_port_is_configurable_in_the_yaml_file_and_overridable_by_the_environment() {
    let h = Home::new("port");
    h.write_config("server:\n  port: 8123\n");
    assert!(out(&h.cmd().arg("env").output().unwrap())
        .contains("ANTHROPIC_BASE_URL=http://127.0.0.1:8123"));
    let o = h
        .cmd()
        .arg("env")
        .env("AIRMATE_PORT", "7777")
        .output()
        .unwrap();
    assert!(
        out(&o).contains("http://127.0.0.1:7777"),
        "the environment beats the file: {}",
        out(&o)
    );
    let shown = out(&h.cmd().args(["config", "show"]).output().unwrap());
    assert!(shown.contains("server.port = 8123  # file"), "{shown}");
    let shown = out(&h
        .cmd()
        .args(["config", "show"])
        .env("AIRMATE_PORT", "7777")
        .output()
        .unwrap());
    assert!(
        shown.contains("server.port = 7777  # environment"),
        "{shown}"
    );
}

#[test]
fn config_show_without_a_file_says_the_port_is_a_default() {
    let h = Home::new("show");
    let shown = out(&h.cmd().args(["config", "show"]).output().unwrap());
    assert!(shown.contains("server.port = 9997  # default"), "{shown}");
    assert!(shown.contains("server.bind = \"127.0.0.1\"  # default"));
}

#[test]
fn config_path_says_where_the_file_is_and_whether_it_exists() {
    let h = Home::new("path");
    let o = out(&h.cmd().args(["config", "path"]).output().unwrap());
    assert!(
        o.contains("airmate.yaml") && o.contains("does not exist"),
        "{o}"
    );
    h.write_config("{}\n");
    assert!(out(&h.cmd().args(["config", "path"]).output().unwrap()).contains("(exists)"));
    let o = out(&h
        .cmd()
        .args(["config", "path"])
        .env("AIRMATE_CONFIG", "/elsewhere/a.yaml")
        .output()
        .unwrap());
    assert!(o.starts_with("/elsewhere/a.yaml"), "{o}");
}

#[test]
fn config_check_validates_resolves_secrets_and_reports_every_problem() {
    let h = Home::new("check");
    h.write_config("upstreams:\n  - name: a\n    url: http://localhost:1\n    api_key: { env: A_KEY }\nmodels:\n  - { match: \"*\", targets: [ { upstream: a, model: m } ] }\n");
    let o = h
        .cmd()
        .args(["config", "check"])
        .env("A_KEY", "secret-value")
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", errs(&o));
    let t = out(&o);
    assert!(
        t.contains("config ok: 1 upstream(s)")
            && t.contains("upstreams[a].api_key resolved from environment variable A_KEY"),
        "{t}"
    );
    assert!(!t.contains("secret-value"), "a secret is never printed");

    let o = h.cmd().args(["config", "check"]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(
        errs(&o).contains("upstreams[a].api_key") && errs(&o).contains("A_KEY"),
        "{}",
        errs(&o)
    );

    h.write_config("server:\n  prot: 1\n");
    let o = h.cmd().args(["config", "check"]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(
        errs(&o).contains("unknown field `prot`") && errs(&o).contains("line 2"),
        "{}",
        errs(&o)
    );

    h.write_config("server:\n  max_concurrent: 0\n  max_body_bytes: 0\n");
    let e = errs(&h.cmd().args(["config", "check"]).output().unwrap());
    assert!(
        e.contains("max_concurrent") && e.contains("max_body_bytes"),
        "{e}"
    );
}

#[test]
fn a_named_config_file_that_is_missing_is_an_error_but_the_default_may_be_absent() {
    let h = Home::new("missing");
    assert!(h
        .cmd()
        .args(["config", "show"])
        .output()
        .unwrap()
        .status
        .success());
    let o = h
        .cmd()
        .args(["--config", "/nonexistent/airmate.yaml", "config", "show"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(errs(&o).contains("cannot read the config file"));
}

#[test]
fn profiles_are_selected_with_the_profile_flag() {
    let h = Home::new("profile");
    h.write_config("server: { port: 8000 }\nprofiles:\n  work:\n    server: { port: 9100 }\n");
    assert!(out(&h.cmd().arg("env").output().unwrap()).contains(":8000"));
    assert!(out(&h.cmd().args(["--profile", "work", "env"]).output().unwrap()).contains(":9100"));
    let o = h.cmd().args(["--profile", "nope", "env"]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(errs(&o).contains("no profile named `nope`"));
}

#[cfg(feature = "completions")]
#[test]
fn completions_print_a_script() {
    let h = Home::new("completions");
    let o = h.cmd().args(["completions", "bash"]).output().unwrap();
    assert!(o.status.success());
    assert!(out(&o).contains("airmate"));
}

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

fn wait_until_listening(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("the gateway did not start listening on {port}");
}

fn http(port: u16, method: &str, path: &str, token: Option<&str>, body: &str) -> (u16, String) {
    use std::io::Write;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let auth = token
        .map(|t| format!("x-api-key: {t}\r\n"))
        .unwrap_or_default();
    write!(s, "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{auth}content-type: application/json\r\ncontent-length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    // A gateway that answers (a 401, say) before it reads the body may close with the body unread,
    // which TCP reports as a reset after the response has already arrived. `read_to_end` keeps
    // what arrived before the error.
    let mut raw = Vec::new();
    if let Err(e) = s.read_to_end(&mut raw) {
        assert!(
            e.kind() == std::io::ErrorKind::ConnectionReset && !raw.is_empty(),
            "reading the reply: {e}"
        );
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (
        status,
        text.split_once("\r\n\r\n")
            .map_or(String::new(), |(_, b)| b.to_owned()),
    )
}

// The test blocks on plain sockets, so the mock upstream needs a worker thread of its own.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn serve_runs_the_gateway_end_to_end_and_stops_on_sigterm() {
    let mock = Mock::start(|_| {
        Reply::json(200, serde_json::json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "from the mock"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}}))
    })
    .await;
    let h = Home::new("serve");
    let port = free_port();
    let mut child = h
        .cmd()
        .args([
            "serve",
            "--port",
            &port.to_string(),
            "--upstream-url",
            &mock.url(),
            "--model",
            "local-model",
        ])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_until_listening(port);

    let token = std::fs::read_to_string(h.token_file()).unwrap();
    let (status, _) = http(port, "POST", "/v1/messages", None, "{}");
    assert_eq!(status, 401, "default-closed: a token is required");
    let body =
        r#"{"model":"claude-x","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#;
    let (status, reply) = http(port, "POST", "/v1/messages", Some(&token), body);
    assert_eq!(status, 200, "{reply}");
    assert!(
        reply.contains("from the mock") && reply.contains("\"model\":\"claude-x\""),
        "{reply}"
    );
    assert_eq!(mock.recorded()[0].json()["model"], "local-model");

    #[cfg(unix)]
    {
        let pid = child.id().to_string();
        assert!(Command::new("kill")
            .args(["-TERM", &pid])
            .status()
            .unwrap()
            .success());
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(s) = child.try_wait().unwrap() {
                break s;
            }
            assert!(Instant::now() < deadline, "serve did not stop on SIGTERM");
            std::thread::sleep(Duration::from_millis(50));
        };
        assert!(status.success());
        let mut stderr = String::new();
        child
            .stderr
            .take()
            .unwrap()
            .read_to_string(&mut stderr)
            .unwrap();
        assert!(
            stderr.contains("listening on http://127.0.0.1:") && stderr.contains("shutting down"),
            "{stderr}"
        );
        assert!(!stderr.contains(&token), "the token is never logged");
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
}

#[test]
fn serve_fails_closed_when_the_port_is_taken() {
    let h = Home::new("taken");
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let o = h
        .cmd()
        .args(["serve", "--port", &port.to_string()])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    assert!(
        errs(&o).contains("cannot listen") && errs(&o).contains("does not fall back"),
        "{}",
        errs(&o)
    );
}

#[test]
fn serve_refuses_a_remote_bind_without_hosts_and_an_acknowledgement() {
    let h = Home::new("remote");
    let o = h
        .cmd()
        .args(["serve", "--bind", "0.0.0.0", "--port", "0"])
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(
        errs(&o).contains("allowed_hosts") && errs(&o).contains("insecure_remote"),
        "{}",
        errs(&o)
    );
}

#[test]
fn serve_refuses_a_group_readable_token_file() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let h = Home::new("loosetoken");
        let o = h.cmd().arg("env").output().unwrap();
        assert!(o.status.success());
        std::fs::set_permissions(h.token_file(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let o = h.cmd().args(["serve", "--port", "0"]).output().unwrap();
        assert_eq!(o.status.code(), Some(1));
        assert!(
            errs(&o).contains("readable by group or others"),
            "{}",
            errs(&o)
        );
    }
}

#[test]
fn run_points_a_command_at_a_private_gateway_and_returns_its_exit_status() {
    #[cfg(unix)]
    {
        let h = Home::new("run");
        let o = h
            .cmd()
            .args([
                "run",
                "--",
                "sh",
                "-c",
                "echo $ANTHROPIC_BASE_URL $OPENAI_BASE_URL",
            ])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", errs(&o));
        let t = out(&o);
        assert!(
            t.starts_with("http://127.0.0.1:") && t.contains("/v1"),
            "{t}"
        );
        assert!(
            !t.contains(":9997"),
            "a private instance runs on a free port: {t}"
        );
        assert!(errs(&o).contains("started a private gateway"));
        let o = h
            .cmd()
            .args(["run", "--", "sh", "-c", "exit 3"])
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(3));
        let o = h
            .cmd()
            .args(["run", "--", "airmate-no-such-program"])
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(1));
        assert!(errs(&o).contains("cannot run"));
    }
}

#[test]
fn run_uses_a_gateway_that_is_already_listening() {
    #[cfg(unix)]
    {
        let h = Home::new("run-existing");
        let port = free_port();
        h.write_config(&format!("server:\n  port: {port}\n"));
        let mut gw = h
            .cmd()
            .args(["serve"])
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        wait_until_listening(port);
        let o = h
            .cmd()
            .args(["run", "--", "sh", "-c", "echo $ANTHROPIC_BASE_URL"])
            .output()
            .unwrap();
        let _ = gw.kill();
        let _ = gw.wait();
        assert!(o.status.success(), "{}", errs(&o));
        assert_eq!(out(&o).trim(), format!("http://127.0.0.1:{port}"));
        assert!(errs(&o).contains("already running"));
    }
}

#[test]
fn a_saved_client_key_is_used_by_env_and_a_missing_one_is_explained() {
    let h = Home::new("clientkey");
    let o = h.cmd().args(["env", "--client", "ci"]).output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(
        errs(&o).contains("no saved key for client `ci`")
            && errs(&o).contains("AIRMATE_CLIENT_KEY_CI"),
        "{}",
        errs(&o)
    );
    let o = h
        .cmd()
        .args(["env", "--client", "ci"])
        .env("AIRMATE_CLIENT_KEY_CI", "am_saved")
        .output()
        .unwrap();
    assert!(
        out(&o).contains("ANTHROPIC_AUTH_TOKEN=am_saved"),
        "{}",
        out(&o)
    );
    let _: &Path = &h.0;
}

const ROUTING_CONFIG: &str = r#"
server:
  rule_headers: [x-team]
upstreams:
  - { name: fw, url: "http://fw.invalid" , protocol: anthropic, flavor: fireworks }
  - { name: local, url: "http://localhost:11434" }
models:
  - match: "claude-opus-*"
    name: big
    targets:
      - { upstream: fw, model: accounts/fireworks/models/kimi-k2p6 }
      - { upstream: local, model: qwen3-coder }
  - match: "claude-*"
    name: small
    targets: [ { upstream: local, model: gpt-4.1 } ]
rules:
  - name: data-team
    when: { header: { x-team: "data-*" } }
    use: small
catalog:
  - { match: "qwen3-coder", context_window: 32768, vision: false }
"#;

fn write_request(h: &Home, name: &str, body: &str) -> String {
    let p = h.0.join(name);
    std::fs::write(&p, body).unwrap();
    p.display().to_string()
}

#[test]
fn route_prints_the_ordered_targets() {
    let h = Home::new("route");
    h.write_config(ROUTING_CONFIG);
    let req = write_request(
        &h,
        "req.json",
        r#"{"model":"claude-opus-4","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    let o = h.cmd().args(["route", &req]).output().unwrap();
    assert!(o.status.success(), "{}", errs(&o));
    assert_eq!(
        out(&o),
        "fw/accounts/fireworks/models/kimi-k2p6\nlocal/qwen3-coder\n"
    );
    let o = h.cmd().args(["route", "--json", &req]).output().unwrap();
    let v: serde_json::Value = serde_json::from_str(&out(&o)).unwrap();
    assert_eq!(v["entry"], "big");
    assert_eq!(v["targets"][0], "fw/accounts/fireworks/models/kimi-k2p6");
    assert_eq!(v["via"], "the glob `claude-opus-*`");
}

#[test]
fn route_explain_shows_every_step_including_skips_and_rules() {
    let h = Home::new("explain");
    h.write_config(ROUTING_CONFIG);
    let image = write_request(
        &h,
        "img.json",
        r#"{"model":"claude-opus-4","max_tokens":10,"messages":[{"role":"user","content":[{"type":"image","source":{"type":"url","url":"https://x/y.png"}}]}]}"#,
    );
    let o = h
        .cmd()
        .args(["route", "--explain", &image])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", errs(&o));
    let t = out(&o);
    assert!(
        t.contains("request     anthropic model=claude-opus-4"),
        "{t}"
    );
    assert!(
        t.contains("none matched; the name resolved to `big` via the glob `claude-opus-*`"),
        "{t}"
    );
    assert!(
        t.contains("filtering   skipped local/qwen3-coder: it cannot read images"),
        "{t}"
    );
    assert!(
        t.contains("order  1    fw/accounts/fireworks/models/kimi-k2p6"),
        "{t}"
    );
    assert!(
        t.contains("not in the catalog: the capability filter is blind"),
        "the Fireworks model has no catalog entry: {t}"
    );
    assert!(t.contains("affinity    conversation "), "{t}");
    let plain = write_request(
        &h,
        "plain.json",
        r#"{"model":"claude-opus-4","max_tokens":10,"messages":[{"role":"user","content":"hi"}]}"#,
    );
    let o = h
        .cmd()
        .args(["route", "--explain", "--header", "x-team=data-eng", &plain])
        .output()
        .unwrap();
    assert!(
        out(&o).contains("`data-team` matched, so it chose the entry `small`"),
        "{}",
        out(&o)
    );
    let o = h
        .cmd()
        .args(["route", "--explain", "--header", "x-other=data-eng", &plain])
        .output()
        .unwrap();
    assert!(
        out(&o).contains("none matched"),
        "a header that is not on the allow-list is invisible to rules: {}",
        out(&o)
    );
}

#[test]
fn route_reports_what_the_gateway_would_refuse() {
    let h = Home::new("route-refuse");
    h.write_config(ROUTING_CONFIG);
    let req = write_request(&h, "ghost.json", r#"{"model":"mystery","messages":[]}"#);
    let o = h.cmd().args(["route", &req]).output().unwrap();
    assert!(
        o.status.success(),
        "a non-family name is a direct upstream model: {}",
        errs(&o)
    );
    assert_eq!(out(&o), "fw/mystery\n", "the first upstream is the default");
    let bad = write_request(&h, "bad.json", "not json");
    assert_eq!(
        h.cmd()
            .args(["route", &bad])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
    assert_eq!(
        h.cmd()
            .args(["route", "/nonexistent.json"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2)
    );
    let o = h
        .cmd()
        .args(["route", "--header", "nope", &req])
        .output()
        .unwrap();
    assert!(errs(&o).contains("NAME=VALUE"));
}

#[test]
fn route_works_without_resolving_secrets_or_calling_anyone() {
    let h = Home::new("route-offline");
    h.write_config("upstreams:\n  - { name: a, url: \"http://a.invalid\", api_key: { env: NEVER_SET } }\nmodels:\n  - { match: \"*\", targets: [ { upstream: a, model: m } ] }\n");
    let req = write_request(
        &h,
        "req.json",
        r#"{"model":"claude-x","max_tokens":1,"messages":[{"role":"user","content":"x"}]}"#,
    );
    assert!(h
        .cmd()
        .args(["route", &req])
        .output()
        .unwrap()
        .status
        .success());
    assert_eq!(
        h.cmd()
            .args(["config", "check"])
            .output()
            .unwrap()
            .status
            .code(),
        Some(2),
        "config check does resolve secrets"
    );
}

#[test]
fn models_lists_targets_with_capabilities_and_marks_blind_spots() {
    let h = Home::new("models");
    h.write_config(ROUTING_CONFIG);
    let o = h.cmd().arg("models").output().unwrap();
    assert!(o.status.success(), "{}", errs(&o));
    let t = out(&o);
    assert!(
        t.contains("claude-opus-*  (entry `big`, strategy Fallback)"),
        "{t}"
    );
    assert!(
        t.contains("1. fw/accounts/fireworks/models/kimi-k2p6  [anthropic]"),
        "{t}"
    );
    assert!(
        t.contains("2. local/qwen3-coder  [chat]  context 32768"),
        "{t}"
    );
    assert!(
        t.contains("3. local/gpt-4.1") || t.contains("1. local/gpt-4.1"),
        "{t}"
    );
    assert!(t.contains("! not in the catalog"), "{t}");
    assert!(t.contains("price unknown"), "no price is invented: {t}");
    assert!(t.contains("catalog as of 2026-10-14"));
}

#[test]
fn models_check_fails_when_the_catalog_is_blind() {
    let h = Home::new("models-check");
    h.write_config(ROUTING_CONFIG);
    let o = h.cmd().args(["models", "--check"]).output().unwrap();
    assert_eq!(o.status.code(), Some(3));
    assert!(
        errs(&o).contains("no catalog entry for") && errs(&o).contains("fw/accounts"),
        "{}",
        errs(&o)
    );
    h.write_config("upstreams:\n  - { name: oa, url: \"http://x\" }\nmodels:\n  - { match: \"*\", targets: [ { upstream: oa, model: gpt-4.1 } ] }\n");
    assert!(h
        .cmd()
        .args(["models", "--check"])
        .output()
        .unwrap()
        .status
        .success());
}
