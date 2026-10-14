//! The listener: a Unix-domain socket, a socket handed over by a service manager, and TLS.

mod support;

use std::sync::Arc;

use airmate::config::load_str;
use airmate::obs::NullSink;
use airmate::router::clock::{OsRandom, SystemClock};
use airmate::secret::{Secret, SystemResolver};
use airmate::server::{bind_listener, bind_with_deps, Listener, Tables};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::json;
use support::{Mock, Reply};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

const TOKEN: &str = "listener-test-token";

fn chat_ok() -> serde_json::Value {
    json!({"id": "c", "model": "m", "choices": [{"index": 0, "message": {"role": "assistant", "content": "over the wire"}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}})
}

fn yaml(mock: &Mock, server: &str) -> String {
    format!(
        "server:\n  port: 0\n  affinity: false\n{server}upstreams:\n  - {{ name: up, url: \"{}\" }}\nmodels:\n  - {{ match: \"*\", targets: [ {{ upstream: up, model: m }} ] }}\n",
        mock.url()
    )
}

fn tables(config: &airmate::config::Config) -> Tables {
    Tables::build(
        Arc::new(config.clone()),
        &SystemResolver::new(&|_| None),
        &Secret::new(TOKEN),
    )
    .unwrap()
    .0
}

/// One request over any byte stream, as a client would send it.
async fn exchange<S>(io: S, host: &str) -> (u16, String)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .unwrap();
    tokio::spawn(conn);
    let body = json!({"model": "claude-x", "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]}).to_string();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("host", host)
        .header("content-type", "application/json")
        .header("x-api-key", TOKEN)
        .body(Full::new(Bytes::from(body)))
        .unwrap();
    let resp = sender.send_request(req).await.unwrap();
    let status = resp.status().as_u16();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(unix)]
#[tokio::test]
async fn a_unix_socket_serves_the_same_api_and_is_removed_on_shutdown() {
    use std::os::unix::fs::PermissionsExt;
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let path = format!("/tmp/am-uds-{}/gw.sock", std::process::id());
    let config = load_str(&yaml(&mock, &format!("  unix_socket: \"{path}\"\n")), None).unwrap();
    let gw = bind_with_deps(
        &config,
        tables(&config),
        Arc::new(NullSink),
        Arc::new(SystemClock::new()),
        Arc::new(OsRandom),
    )
    .await
    .unwrap();
    assert_eq!(gw.listen_url(), format!("unix:{path}"));
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(gw.serve(rx));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    let (status, body) = exchange(stream, "localhost").await;
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("over the wire"));
    // The guard still applies: a Host that is not loopback is refused even on a Unix socket.
    let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
    assert_eq!(exchange(stream, "evil.example").await.0, 403);

    stop.send(true).unwrap();
    task.await.unwrap();
    assert!(
        !std::path::Path::new(&path).exists(),
        "the socket file is removed"
    );
    let _ = std::fs::remove_dir_all(std::path::Path::new(&path).parent().unwrap());
}

#[tokio::test]
async fn a_socket_passed_in_by_a_service_manager_is_used_as_is() {
    let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let addr = std_listener.local_addr().unwrap();
    let config = load_str(&yaml(&mock, ""), None).unwrap();
    let listener = Listener::from_inherited(std_listener).unwrap();
    let gw = bind_listener(
        &config,
        listener,
        tables(&config),
        Arc::new(NullSink),
        Arc::new(SystemClock::new()),
        Arc::new(OsRandom),
    )
    .unwrap();
    assert_eq!(
        gw.local_addr(),
        addr,
        "it serves on the address it was given, not the configured one"
    );
    let (stop, rx) = watch::channel(false);
    let task = tokio::spawn(gw.serve(rx));
    let stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let (status, body) = exchange(stream, &format!("127.0.0.1:{}", addr.port())).await;
    assert_eq!(status, 200, "{body}");
    stop.send(true).unwrap();
    task.await.unwrap();
}

#[cfg(all(feature = "tls", unix))]
mod tls {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// A throwaway self-signed certificate for localhost, made with the `openssl` command so no
    /// private key has to be committed to the repository.
    fn make_cert(dir: &Path) -> (PathBuf, PathBuf) {
        std::fs::create_dir_all(dir).unwrap();
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        let out = Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "2",
                "-subj",
                "/CN=localhost",
                "-addext",
                "subjectAltName=DNS:localhost,IP:127.0.0.1",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .output()
            .expect("the `openssl` command is needed to make a test certificate");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        (cert, key)
    }

    fn connector(cert: &Path) -> tokio_rustls::TlsConnector {
        use rustls::pki_types::pem::PemObject;
        let mut roots = rustls::RootCertStore::empty();
        for c in rustls::pki_types::CertificateDer::pem_file_iter(cert).unwrap() {
            roots.add(c.unwrap()).unwrap();
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(roots)
            .with_no_client_auth();
        tokio_rustls::TlsConnector::from(Arc::new(config))
    }

    #[tokio::test]
    async fn https_is_served_with_the_configured_certificate() {
        let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
        let dir = PathBuf::from(format!("/tmp/am-tls-{}", std::process::id()));
        let (cert, key) = make_cert(&dir);
        let server = format!(
            "  tls: {{ cert: '{}', key: '{}' }}\n",
            cert.display(),
            key.display()
        );
        let config = load_str(&yaml(&mock, &server), None).unwrap();
        let gw = bind_with_deps(
            &config,
            tables(&config),
            Arc::new(NullSink),
            Arc::new(SystemClock::new()),
            Arc::new(OsRandom),
        )
        .await
        .unwrap();
        assert!(
            gw.listen_url().starts_with("https://"),
            "{}",
            gw.listen_url()
        );
        let addr = gw.local_addr();
        let (stop, rx) = watch::channel(false);
        let task = tokio::spawn(gw.serve(rx));

        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        let tls = connector(&cert)
            .connect(name, tcp)
            .await
            .expect("the handshake succeeds against the configured certificate");
        let (status, body) = exchange(tls, &format!("localhost:{}", addr.port())).await;
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("over the wire"));

        // Plain HTTP to a TLS port gets no answer, and a client that does not trust the
        // certificate refuses it.
        let mut plain = tokio::net::TcpStream::connect(addr).await.unwrap();
        tokio::io::AsyncWriteExt::write_all(
            &mut plain,
            b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
        )
        .await
        .unwrap();
        let mut buf = Vec::new();
        let _ = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::io::AsyncReadExt::read_to_end(&mut plain, &mut buf),
        )
        .await;
        assert!(
            !String::from_utf8_lossy(&buf).contains("HTTP/1.1 200"),
            "no plaintext answer on a TLS port"
        );
        let empty = rustls::RootCertStore::empty();
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let strict = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_root_certificates(empty)
            .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
        assert!(tokio_rustls::TlsConnector::from(Arc::new(strict))
            .connect(name, tcp)
            .await
            .is_err());

        stop.send(true).unwrap();
        task.await.unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unreadable_or_mismatched_tls_material_stops_the_gateway_from_starting() {
        let mock = Mock::start(|_| Reply::json(200, chat_ok())).await;
        let dir = PathBuf::from(format!("/tmp/am-tls-bad-{}", std::process::id()));
        let (cert, key) = make_cert(&dir);
        let (other_cert, _) = {
            let d2 = dir.join("other");
            make_cert(&d2)
        };
        for (c, k, want) in [
            (
                dir.join("missing.pem"),
                key.clone(),
                "cannot read the TLS certificate",
            ),
            (
                cert.clone(),
                dir.join("missing.pem"),
                "cannot read the TLS key",
            ),
            (other_cert, key.clone(), "do not fit together"),
        ] {
            let server = format!(
                "  tls: {{ cert: '{}', key: '{}' }}\n",
                c.display(),
                k.display()
            );
            let config = load_str(&yaml(&mock, &server), None).unwrap();
            let err = bind_with_deps(
                &config,
                tables(&config),
                Arc::new(NullSink),
                Arc::new(SystemClock::new()),
                Arc::new(OsRandom),
            )
            .await
            .err()
            .expect("binding fails");
            assert!(err.to_string().contains(want), "{err}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
