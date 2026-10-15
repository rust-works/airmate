//! The request guard.
//!
//! Every request passes one guard before any body is read, in this order:
//!
//! 1. a loopback or allowed `Host` for the bound port, which defeats DNS rebinding;
//! 2. no `Origin` header and no `Sec-Fetch-Site` of `cross-site`, `same-site` or `same-origin`, so
//!    a web page the operator visits cannot reach the gateway;
//! 3. a valid client token.
//!
//! The first two fail with `403 permission_error`, the token with `401 authentication_error`.

use airmate_core::error::GatewayError;

/// What the guard needs to know about the listener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GuardConfig {
    /// The port actually bound; `None` for a Unix socket, where any port is accepted.
    pub bound_port: Option<u16>,
    /// `Host` values accepted besides loopback, as `host` or `host:port`.
    pub allowed_hosts: Vec<String>,
}

const LOOPBACK_NAMES: [&str; 4] = ["127.0.0.1", "localhost", "[::1]", "::1"];

/// Splits `host[:port]`, understanding bracketed IPv6.
fn split_host(h: &str) -> (&str, Option<&str>) {
    if let Some(rest) = h.strip_prefix('[') {
        if let Some((addr, after)) = rest.split_once(']') {
            let host = &h[..addr.len() + 2];
            return (host, after.strip_prefix(':'));
        }
        return (h, None);
    }
    match h.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => (host, Some(port)),
        _ => (h, None),
    }
}

/// Checks the `Host` header against the bound authority.
///
/// # Errors
///
/// `403 permission_error` when the host is missing or not allowed.
pub fn check_host(host: Option<&str>, g: &GuardConfig) -> Result<(), GatewayError> {
    let Some(host) = host else {
        return Err(GatewayError::forbidden("a Host header is required"));
    };
    let lower = host.to_ascii_lowercase();
    let (name, port) = split_host(&lower);
    let port_ok = match (g.bound_port, port) {
        (None, _) => true,
        (Some(bound), Some(p)) => p.parse::<u16>().is_ok_and(|p| p == bound),
        (Some(_), None) => false,
    };
    if LOOPBACK_NAMES.contains(&name) && port_ok {
        return Ok(());
    }
    let allowed = g.allowed_hosts.iter().any(|a| {
        let a = a.to_ascii_lowercase();
        a == lower || (a == name && port_ok)
    });
    if allowed {
        Ok(())
    } else {
        Err(GatewayError::forbidden(format!(
            "the Host `{host}` is not allowed by airmate"
        )))
    }
}

/// Refuses browser-originated requests.
///
/// # Errors
///
/// `403 permission_error` when `Origin` is present or `Sec-Fetch-Site` says the request came from
/// a web page.
pub fn check_origin(
    origin: Option<&str>,
    sec_fetch_site: Option<&str>,
) -> Result<(), GatewayError> {
    if origin.is_some() {
        return Err(GatewayError::forbidden(
            "requests from web pages are refused (Origin header present)",
        ));
    }
    if let Some(site) = sec_fetch_site {
        if matches!(
            site.to_ascii_lowercase().as_str(),
            "cross-site" | "same-site" | "same-origin"
        ) {
            return Err(GatewayError::forbidden(
                "requests from web pages are refused (Sec-Fetch-Site)",
            ));
        }
    }
    Ok(())
}

/// The token a request presents: `x-api-key`, else `Authorization: Bearer`.
#[must_use]
pub fn extract_token<'a>(
    x_api_key: Option<&'a str>,
    authorization: Option<&'a str>,
) -> Option<&'a str> {
    if let Some(k) = x_api_key.map(str::trim).filter(|k| !k.is_empty()) {
        return Some(k);
    }
    let a = authorization?.trim();
    let (scheme, token) = a.split_once(char::is_whitespace)?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use airmate_core::error::ErrorKind;

    fn g(port: Option<u16>, allowed: &[&str]) -> GuardConfig {
        GuardConfig {
            bound_port: port,
            allowed_hosts: allowed.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    fn status(r: Result<(), GatewayError>) -> Option<u16> {
        r.err().map(|e| e.status())
    }

    #[test]
    fn loopback_hosts_on_the_bound_port_pass() {
        let guard = g(Some(9997), &[]);
        for h in [
            "127.0.0.1:9997",
            "localhost:9997",
            "LOCALHOST:9997",
            "[::1]:9997",
        ] {
            assert_eq!(status(check_host(Some(h), &guard)), None, "{h}");
        }
    }

    #[test]
    fn the_wrong_port_or_a_foreign_host_is_refused_defeating_dns_rebinding() {
        let guard = g(Some(9997), &[]);
        for h in [
            "127.0.0.1:80",
            "127.0.0.1",
            "localhost:abc",
            "evil.example:9997",
            "evil.example",
            "127.0.0.1.evil.example:9997",
            "0.0.0.0:9997",
            "",
        ] {
            assert_eq!(status(check_host(Some(h), &guard)), Some(403), "{h:?}");
        }
        assert_eq!(
            status(check_host(None, &guard)),
            Some(403),
            "a missing Host"
        );
    }

    #[test]
    fn allowed_hosts_extend_the_list() {
        let guard = g(Some(9997), &["airmate.example", "box.lan:9997"]);
        assert_eq!(
            status(check_host(Some("airmate.example:9997"), &guard)),
            None
        );
        assert_eq!(
            status(check_host(Some("AIRMATE.example:9997"), &guard)),
            None
        );
        assert_eq!(status(check_host(Some("box.lan:9997"), &guard)), None);
        assert_eq!(status(check_host(Some("box.lan:1"), &guard)), Some(403));
        assert_eq!(
            status(check_host(Some("airmate.example:1"), &guard)),
            Some(403),
            "the port must still match"
        );
        assert_eq!(
            status(check_host(Some("other.example:9997"), &guard)),
            Some(403)
        );
    }

    #[test]
    fn a_unix_socket_accepts_loopback_names_on_any_port() {
        let guard = g(None, &[]);
        assert_eq!(status(check_host(Some("localhost"), &guard)), None);
        assert_eq!(status(check_host(Some("localhost:1234"), &guard)), None);
        assert_eq!(status(check_host(Some("evil.example"), &guard)), Some(403));
    }

    #[test]
    fn browser_requests_are_refused() {
        assert_eq!(
            status(check_origin(Some("https://evil.example"), None)),
            Some(403)
        );
        assert_eq!(status(check_origin(Some("null"), None)), Some(403));
        for site in ["cross-site", "same-site", "same-origin", "Cross-Site"] {
            assert_eq!(status(check_origin(None, Some(site))), Some(403), "{site}");
        }
        assert_eq!(
            status(check_origin(None, Some("none"))),
            None,
            "a user typing the URL is not a page"
        );
        assert_eq!(status(check_origin(None, None)), None);
    }

    #[test]
    fn guard_failures_are_permission_errors() {
        assert_eq!(
            check_origin(Some("x"), None).unwrap_err().kind,
            ErrorKind::Permission
        );
        assert_eq!(
            check_host(None, &g(Some(1), &[])).unwrap_err().kind,
            ErrorKind::Permission
        );
    }

    #[test]
    fn the_token_is_read_from_x_api_key_or_a_bearer_header() {
        assert_eq!(extract_token(Some("k1"), None), Some("k1"));
        assert_eq!(extract_token(None, Some("Bearer k2")), Some("k2"));
        assert_eq!(extract_token(None, Some("bearer   k3 ")), Some("k3"));
        assert_eq!(
            extract_token(Some("k1"), Some("Bearer k2")),
            Some("k1"),
            "x-api-key wins"
        );
        assert_eq!(extract_token(Some("  "), Some("Bearer k2")), Some("k2"));
        for a in ["Basic abc", "Bearer", "Bearer  ", "k", ""] {
            assert_eq!(extract_token(None, Some(a)), None, "{a:?}");
        }
        assert_eq!(extract_token(None, None), None);
    }

    #[test]
    fn hosts_split_with_and_without_ports_and_brackets() {
        assert_eq!(split_host("a:1"), ("a", Some("1")));
        assert_eq!(split_host("a"), ("a", None));
        assert_eq!(split_host("[::1]:5"), ("[::1]", Some("5")));
        assert_eq!(split_host("[::1]"), ("[::1]", None));
        assert_eq!(split_host("::1"), ("::1", None));
        assert_eq!(split_host("[bad"), ("[bad", None));
    }
}
