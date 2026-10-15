//! What the gateway listens on: a TCP port, a Unix-domain socket, or a socket a service manager
//! already opened, with TLS in front of TCP when configured.

use std::net::SocketAddr;
use std::path::PathBuf;

use tokio::net::{TcpListener, TcpStream};

use crate::config::schema::ServerConfig;

/// Why binding failed.
#[derive(Debug)]
pub struct BindError(pub String);

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BindError {}

/// A bound listener.
pub enum Listener {
    /// TCP.
    Tcp(TcpListener),
    /// A Unix-domain socket, and where it lives so it can be removed on exit.
    #[cfg(unix)]
    Unix(tokio::net::UnixListener, PathBuf),
}

/// A connection accepted from a [`Listener`].
pub enum Accepted {
    /// TCP.
    Tcp(TcpStream),
    /// A Unix-domain socket.
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
}

impl Listener {
    /// Binds what `server` asks for: the Unix socket if one is configured, else TCP. Fails closed:
    /// a taken port is an error, never a fallback to another one.
    ///
    /// # Errors
    ///
    /// The address is invalid or cannot be bound.
    pub async fn bind(server: &ServerConfig) -> Result<Self, BindError> {
        if let Some(path) = &server.unix_socket {
            return Self::bind_unix(path);
        }
        let addr: SocketAddr = format!(
            "{}:{}",
            if server.bind.contains(':') {
                format!("[{}]", server.bind)
            } else {
                server.bind.clone()
            },
            server.port
        )
        .parse()
        .map_err(|e| BindError(format!("invalid listen address: {e}")))?;
        let listener = TcpListener::bind(addr).await.map_err(|e| {
            BindError(format!(
                "cannot listen on {addr}: {e}{}",
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    "; airmate does not fall back to another port (use --port 0 to ask for any free port)"
                } else {
                    ""
                }
            ))
        })?;
        Ok(Self::Tcp(listener))
    }

    #[cfg(unix)]
    fn bind_unix(path: &str) -> Result<Self, BindError> {
        let path = PathBuf::from(path);
        crate::sys::bind_private_socket(&path)
            .map(|l| Self::Unix(l, path.clone()))
            .map_err(|e| BindError(format!("cannot listen on {}: {e}", path.display())))
    }

    #[cfg(not(unix))]
    fn bind_unix(_: &str) -> Result<Self, BindError> {
        Err(BindError(
            "`server.unix_socket` needs Unix-domain sockets, which this platform does not offer"
                .to_owned(),
        ))
    }

    /// Adopts a TCP socket a service manager already opened.
    ///
    /// # Errors
    ///
    /// The socket cannot be registered with the runtime.
    pub fn from_inherited(listener: std::net::TcpListener) -> Result<Self, BindError> {
        TcpListener::from_std(listener).map(Self::Tcp).map_err(|e| {
            BindError(format!(
                "cannot use the socket the service manager passed: {e}"
            ))
        })
    }

    /// Waits for the next connection.
    ///
    /// # Errors
    ///
    /// The accept call failed (for example `EMFILE`).
    pub async fn accept(&self) -> std::io::Result<Accepted> {
        match self {
            Self::Tcp(l) => l.accept().await.map(|(s, _)| Accepted::Tcp(s)),
            #[cfg(unix)]
            Self::Unix(l, _) => l.accept().await.map(|(s, _)| Accepted::Unix(s)),
        }
    }

    /// The TCP address, `127.0.0.1:0` for a Unix socket (which has no port).
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        match self {
            Self::Tcp(l) => l
                .local_addr()
                .unwrap_or_else(|_| SocketAddr::from(([127, 0, 0, 1], 0))),
            #[cfg(unix)]
            Self::Unix(..) => SocketAddr::from(([127, 0, 0, 1], 0)),
        }
    }

    /// The TCP port the guard checks `Host` against; none for a Unix socket.
    #[must_use]
    pub fn port(&self) -> Option<u16> {
        match self {
            Self::Tcp(_) => Some(self.local_addr().port()),
            #[cfg(unix)]
            Self::Unix(..) => None,
        }
    }

    /// Where it listens, for messages: `http://127.0.0.1:9997` or `unix:/path`.
    #[must_use]
    pub fn describe(&self, tls: bool) -> String {
        match self {
            Self::Tcp(_) => format!(
                "{}://{}",
                if tls { "https" } else { "http" },
                self.local_addr()
            ),
            #[cfg(unix)]
            Self::Unix(_, p) => format!("unix:{}", p.display()),
        }
    }

    /// Removes the socket file of a Unix listener.
    pub fn cleanup(&self) {
        #[cfg(unix)]
        if let Self::Unix(_, p) = self {
            let _ = std::fs::remove_file(p);
        }
    }
}

/// Builds the TLS acceptor for `server.tls`.
///
/// # Errors
///
/// The certificate or key cannot be read or parsed, or TLS is not built in.
#[cfg(feature = "tls")]
pub fn tls_acceptor(
    cfg: &crate::config::schema::TlsConfig,
) -> Result<tokio_rustls::TlsAcceptor, BindError> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use std::sync::Arc;
    let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(&cfg.cert)
        .map_err(|e| BindError(format!("cannot read the TLS certificate {}: {e}", cfg.cert)))?
        .collect::<Result<_, _>>()
        .map_err(|e| BindError(format!("bad TLS certificate {}: {e}", cfg.cert)))?;
    if certs.is_empty() {
        return Err(BindError(format!("{} holds no certificate", cfg.cert)));
    }
    let key = PrivateKeyDer::from_pem_file(&cfg.key)
        .map_err(|e| BindError(format!("cannot read the TLS key {}: {e}", cfg.key)))?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| BindError(format!("TLS protocol setup failed: {e}")))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| {
            BindError(format!(
                "the TLS certificate and key do not fit together: {e}"
            ))
        })?;
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}
