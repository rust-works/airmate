//! AWS Signature Version 4, for calling Bedrock with an access key.
//!
//! Pure: the caller supplies the timestamp, so the same input always signs the same way and the
//! core keeps no clock. Only what Bedrock needs is here: header-based signing of a request whose
//! payload is in memory.

use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// The credentials to sign with.
#[derive(Debug, Clone, Copy)]
pub struct Credentials<'a> {
    /// The access key id (`AKIA…`).
    pub access_key_id: &'a str,
    /// The secret access key.
    pub secret_access_key: &'a str,
    /// The session token, for temporary credentials.
    pub session_token: Option<&'a str>,
}

/// What is being signed.
#[derive(Debug, Clone, Copy)]
pub struct Request<'a> {
    /// `POST`, `GET`…
    pub method: &'a str,
    /// The `Host` header: the host, plus `:port` when it is not the default.
    pub host: &'a str,
    /// The path as sent on the wire, with each segment already percent-encoded once.
    pub path: &'a str,
    /// The query string as sent, or empty.
    pub query: &'a str,
    /// The payload.
    pub body: &'a [u8],
    /// The region, e.g. `us-east-1`.
    pub region: &'a str,
    /// The service signing name; `bedrock` for the runtime API.
    pub service: &'a str,
    /// The timestamp as `YYYYMMDDTHHMMSSZ`.
    pub amz_date: &'a str,
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

fn sha256_hex(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut m =
        <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC takes a key of any length");
    m.update(data);
    m.finalize().into_bytes().to_vec()
}

/// Percent-encodes everything but the unreserved characters `A-Za-z0-9-_.~`, and `/` when
/// `keep_slash`.
fn encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric()
            || matches!(b, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && b == b'/')
        {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encodes one path segment (a model id, say) for the wire.
#[must_use]
pub fn encode_segment(s: &str) -> String {
    encode(s, false)
}

fn canonical_query(query: &str) -> String {
    if query.is_empty() {
        return String::new();
    }
    let mut pairs: Vec<(String, String)> = query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (k.to_owned(), v.to_owned())
        })
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// `YYYYMMDDTHHMMSSZ` for a Unix time, as SigV4 wants it.
#[must_use]
pub fn amz_date(unix_secs: u64) -> String {
    let days = i64::try_from(unix_secs / 86_400).unwrap_or(i64::MAX);
    let rem = unix_secs % 86_400;
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}{m:02}{d:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The headers to add to the request: `x-amz-date`, `x-amz-security-token` when there is a
/// session token, and `authorization`.
#[must_use]
pub fn sign(creds: &Credentials<'_>, r: &Request<'_>) -> Vec<(String, String)> {
    let date = &r.amz_date[..8.min(r.amz_date.len())];
    let mut headers: Vec<(&str, String)> = vec![
        ("host", r.host.to_owned()),
        ("x-amz-date", r.amz_date.to_owned()),
    ];
    if let Some(t) = creds.session_token {
        headers.push(("x-amz-security-token", t.to_owned()));
    }
    headers.sort_by(|a, b| a.0.cmp(b.0));
    let canonical_headers: String = headers
        .iter()
        .map(|(k, v)| format!("{k}:{}\n", v.trim()))
        .collect();
    let signed_headers = headers
        .iter()
        .map(|(k, _)| *k)
        .collect::<Vec<_>>()
        .join(";");

    // Outside S3 the path is encoded a second time in the canonical request.
    let canonical_uri = encode(r.path, true);
    let canonical_request = format!(
        "{}\n{}\n{}\n{}\n{}\n{}",
        r.method,
        canonical_uri,
        canonical_query(r.query),
        canonical_headers,
        signed_headers,
        sha256_hex(r.body)
    );
    let scope = format!("{date}/{}/{}/aws4_request", r.region, r.service);
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{}\n{scope}\n{}",
        r.amz_date,
        sha256_hex(canonical_request.as_bytes())
    );
    let k_date = hmac(
        format!("AWS4{}", creds.secret_access_key).as_bytes(),
        date.as_bytes(),
    );
    let k_region = hmac(&k_date, r.region.as_bytes());
    let k_service = hmac(&k_region, r.service.as_bytes());
    let k_signing = hmac(&k_service, b"aws4_request");
    let signature = hex(&hmac(&k_signing, to_sign.as_bytes()));

    let mut out = vec![("x-amz-date".to_owned(), r.amz_date.to_owned())];
    if let Some(t) = creds.session_token {
        out.push(("x-amz-security-token".to_owned(), t.to_owned()));
    }
    out.push((
        "authorization".to_owned(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            creds.access_key_id
        ),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const CREDS: Credentials<'static> = Credentials {
        access_key_id: "AKIDEXAMPLE",
        secret_access_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        session_token: None,
    };

    fn header<'a>(h: &'a [(String, String)], name: &str) -> &'a str {
        h.iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    /// AWS's published `get-vanilla` case from the Signature Version 4 test suite.
    #[test]
    fn the_aws_get_vanilla_vector_signs_to_the_published_signature() {
        let h = sign(
            &CREDS,
            &Request {
                method: "GET",
                host: "example.amazonaws.com",
                path: "/",
                query: "",
                body: b"",
                region: "us-east-1",
                service: "service",
                amz_date: "20150830T123600Z",
            },
        );
        assert_eq!(
            header(&h, "authorization"),
            "AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/service/aws4_request, SignedHeaders=host;x-amz-date, Signature=5fa00fa31553b73ebf1942676e86291e8372ff2a2260956d9b8aae1d763fbf31"
        );
        assert_eq!(header(&h, "x-amz-date"), "20150830T123600Z");
    }

    /// The `post-x-www-form-urlencoded` case: a body is part of what is signed.
    #[test]
    fn the_body_hash_changes_the_signature() {
        let req = |body: &'static [u8]| Request {
            method: "POST",
            host: "example.amazonaws.com",
            path: "/",
            query: "",
            body,
            region: "us-east-1",
            service: "service",
            amz_date: "20150830T123600Z",
        };
        let a = sign(&CREDS, &req(b"Param1=value1"));
        let b = sign(&CREDS, &req(b"Param1=value2"));
        assert_ne!(header(&a, "authorization"), header(&b, "authorization"));
        assert_eq!(
            header(&a, "authorization"),
            header(&sign(&CREDS, &req(b"Param1=value1")), "authorization"),
            "signing is deterministic"
        );
    }

    #[test]
    fn a_session_token_is_signed_and_returned() {
        let creds = Credentials {
            session_token: Some("tok/en+=="),
            ..CREDS
        };
        let h = sign(
            &creds,
            &Request {
                method: "POST",
                host: "bedrock-runtime.us-east-1.amazonaws.com",
                path: "/model/x/converse",
                query: "",
                body: b"{}",
                region: "us-east-1",
                service: "bedrock",
                amz_date: "20260101T000000Z",
            },
        );
        assert_eq!(header(&h, "x-amz-security-token"), "tok/en+==");
        assert!(header(&h, "authorization")
            .contains("SignedHeaders=host;x-amz-date;x-amz-security-token"));
        assert!(header(&h, "authorization")
            .contains("Credential=AKIDEXAMPLE/20260101/us-east-1/bedrock/aws4_request"));
    }

    #[test]
    fn the_wire_path_is_encoded_once_and_the_canonical_path_twice() {
        let id = "anthropic.claude-3-5-sonnet-20241022-v2:0";
        assert_eq!(
            encode_segment(id),
            "anthropic.claude-3-5-sonnet-20241022-v2%3A0"
        );
        assert_eq!(
            encode("/model/anthropic.claude-v2%3A0/converse", true),
            "/model/anthropic.claude-v2%253A0/converse"
        );
        assert_eq!(
            encode_segment("arn:aws:bedrock:us-east-1:123:inference-profile/us.x"),
            "arn%3Aaws%3Abedrock%3Aus-east-1%3A123%3Ainference-profile%2Fus.x"
        );
    }

    #[test]
    fn a_different_path_or_host_signs_differently() {
        let base = Request {
            method: "POST",
            host: "h.example.com",
            path: "/a",
            query: "",
            body: b"",
            region: "r",
            service: "s",
            amz_date: "20260101T000000Z",
        };
        let sig = |r: Request<'_>| header(&sign(&CREDS, &r), "authorization").to_owned();
        let s0 = sig(base);
        assert_ne!(s0, sig(Request { path: "/b", ..base }));
        assert_ne!(
            s0,
            sig(Request {
                host: "other.example.com",
                ..base
            })
        );
        assert_ne!(
            s0,
            sig(Request {
                query: "x=1",
                ..base
            })
        );
        assert_ne!(
            s0,
            sig(Request {
                amz_date: "20260101T000001Z",
                ..base
            })
        );
    }

    #[test]
    fn timestamps_are_formatted_the_way_sigv4_wants() {
        assert_eq!(amz_date(0), "19700101T000000Z");
        assert_eq!(
            amz_date(1_440_938_160),
            "20150830T123600Z",
            "the get-vanilla instant"
        );
        assert_eq!(amz_date(951_782_400), "20000229T000000Z");
        assert_eq!(amz_date(4_102_444_799), "20991231T235959Z");
    }

    #[test]
    fn query_parameters_are_sorted_in_the_canonical_form() {
        assert_eq!(canonical_query("b=2&a=1&c="), "a=1&b=2&c=");
        assert_eq!(canonical_query(""), "");
    }
}
