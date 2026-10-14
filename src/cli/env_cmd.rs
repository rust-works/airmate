//! `airmate env` and the connection settings shared with `airmate run`.

use std::collections::BTreeMap;
use std::io::Write;

use serde_json::json;

use super::common::CliError;
use crate::config::schema::Config;

/// The inbound protocol family whose client settings to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    /// Anthropic SDKs and Claude Code.
    Anthropic,
    /// OpenAI SDKs.
    OpenAi,
}

/// The host a client should use to reach the listener.
#[must_use]
pub fn client_host(bind: &str) -> String {
    match bind {
        "0.0.0.0" => "127.0.0.1".to_owned(),
        "::" => "[::1]".to_owned(),
        b if b.contains(':') => format!("[{b}]"),
        b => b.to_owned(),
    }
}

/// The base URL clients use.
///
/// # Errors
///
/// The port is 0, so there is no fixed address to print.
pub fn base_url(config: &Config) -> Result<String, CliError> {
    if config.server.port == 0 {
        return Err(CliError::usage(
            "server.port is 0 (any free port), so there is no fixed address to print; set a port",
        ));
    }
    Ok(format!(
        "http://{}:{}",
        client_host(&config.server.bind),
        config.server.port
    ))
}

/// The variables a client of `family` needs.
#[must_use]
pub fn variables(family: Family, base: &str, token: &str) -> BTreeMap<String, String> {
    let mut v = BTreeMap::new();
    match family {
        Family::Anthropic => {
            v.insert("ANTHROPIC_BASE_URL".to_owned(), base.to_owned());
            v.insert("ANTHROPIC_AUTH_TOKEN".to_owned(), token.to_owned());
        }
        Family::OpenAi => {
            v.insert("OPENAI_BASE_URL".to_owned(), format!("{base}/v1"));
            v.insert("OPENAI_API_KEY".to_owned(), token.to_owned());
        }
    }
    v
}

/// Single-quotes a value for a POSIX shell.
#[must_use]
pub fn shell_quote(v: &str) -> String {
    if !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,".contains(c))
    {
        v.to_owned()
    } else {
        format!("'{}'", v.replace('\'', "'\\''"))
    }
}

/// The output format of `airmate env`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// `export NAME=value`
    Shell,
    /// `{"env": {...}}`
    Json,
    /// `NAME=value`
    Dotenv,
}

/// Prints `vars` in `format`.
///
/// # Errors
///
/// A write failure.
pub fn render(
    vars: &BTreeMap<String, String>,
    format: Format,
    out: &mut dyn Write,
) -> std::io::Result<()> {
    match format {
        Format::Shell => {
            for (k, v) in vars {
                writeln!(out, "export {k}={}", shell_quote(v))?;
            }
        }
        Format::Dotenv => {
            for (k, v) in vars {
                writeln!(out, "{k}={v}")?;
            }
        }
        Format::Json => {
            writeln!(
                out,
                "{}",
                serde_json::to_string_pretty(&json!({ "env": vars })).expect("serialises")
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(vars: &BTreeMap<String, String>, f: Format) -> String {
        let mut out = Vec::new();
        render(vars, f, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_default_base_url_uses_port_9997() {
        assert_eq!(
            base_url(&Config::default()).unwrap(),
            "http://127.0.0.1:9997"
        );
    }

    #[test]
    fn the_base_url_follows_the_configured_port_and_bind() {
        let mut c = Config::default();
        c.server.port = 8123;
        assert_eq!(base_url(&c).unwrap(), "http://127.0.0.1:8123");
        c.server.bind = "::1".into();
        assert_eq!(base_url(&c).unwrap(), "http://[::1]:8123");
        c.server.bind = "0.0.0.0".into();
        assert_eq!(
            base_url(&c).unwrap(),
            "http://127.0.0.1:8123",
            "a wildcard bind is reached via loopback"
        );
        c.server.port = 0;
        assert!(base_url(&c).unwrap_err().message.contains("port is 0"));
    }

    #[test]
    fn each_family_gets_its_own_variables() {
        let a = variables(Family::Anthropic, "http://127.0.0.1:9997", "tok");
        assert_eq!(a["ANTHROPIC_BASE_URL"], "http://127.0.0.1:9997");
        assert_eq!(a["ANTHROPIC_AUTH_TOKEN"], "tok");
        assert_eq!(a.len(), 2);
        let o = variables(Family::OpenAi, "http://127.0.0.1:9997", "tok");
        assert_eq!(o["OPENAI_BASE_URL"], "http://127.0.0.1:9997/v1");
        assert_eq!(o["OPENAI_API_KEY"], "tok");
    }

    #[test]
    fn shell_output_quotes_what_needs_it() {
        let mut v = BTreeMap::new();
        v.insert("A".to_owned(), "plain-value_1".to_owned());
        v.insert("B".to_owned(), "it's got spaces".to_owned());
        v.insert("C".to_owned(), String::new());
        assert_eq!(
            text(&v, Format::Shell),
            "export A=plain-value_1\nexport B='it'\\''s got spaces'\nexport C=''\n"
        );
    }

    #[test]
    fn dotenv_and_json_formats() {
        let vars = variables(Family::Anthropic, "http://h:1", "t");
        assert_eq!(
            text(&vars, Format::Dotenv),
            "ANTHROPIC_AUTH_TOKEN=t\nANTHROPIC_BASE_URL=http://h:1\n"
        );
        let j: serde_json::Value = serde_json::from_str(&text(&vars, Format::Json)).unwrap();
        assert_eq!(j["env"]["ANTHROPIC_BASE_URL"], "http://h:1");
    }
}
