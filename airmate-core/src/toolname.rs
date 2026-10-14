//! Tool-name shortening.
//!
//! OpenAI-protocol upstreams cap function names at 64 characters, which long MCP tool names
//! exceed. A longer name is shortened to its first 55 characters, `_`, and 8 hex digits of the
//! SHA-256 of the full name. The mapping is kept per request and applied in reverse to the reply,
//! including streamed tool-call starts.

use std::collections::HashMap;

use sha2::{Digest, Sha256};

/// The longest name upstreams accept.
pub const MAX_NAME_CHARS: usize = 64;
const KEEP_CHARS: usize = 55;

/// A per-request mapping between full and shortened tool names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolNames {
    short_to_full: HashMap<String, String>,
}

impl ToolNames {
    /// An empty mapping.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the name to send upstream for `full`, remembering the mapping if it was shortened.
    pub fn shorten(&mut self, full: &str) -> String {
        if full.chars().count() <= MAX_NAME_CHARS {
            return full.to_owned();
        }
        let digest = Sha256::digest(full.as_bytes());
        let hex: String = digest.iter().take(4).map(|b| format!("{b:02x}")).collect();
        let prefix: String = full.chars().take(KEEP_CHARS).collect();
        let short = format!("{prefix}_{hex}");
        self.short_to_full.insert(short.clone(), full.to_owned());
        short
    }

    /// Returns the name the client knows for an upstream name.
    #[must_use]
    pub fn restore<'a>(&'a self, name: &'a str) -> &'a str {
        self.short_to_full.get(name).map_or(name, String::as_str)
    }

    /// Whether any name was shortened.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.short_to_full.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_pass_through_untouched() {
        let mut n = ToolNames::new();
        let name = "a".repeat(64);
        assert_eq!(n.shorten(&name), name);
        assert_eq!(n.shorten("read_file"), "read_file");
        assert!(n.is_empty());
        assert_eq!(n.restore("read_file"), "read_file");
    }

    #[test]
    fn long_names_become_55_chars_underscore_and_8_hex_digits() {
        let mut n = ToolNames::new();
        let full = format!("mcp__server__{}", "x".repeat(80));
        let short = n.shorten(&full);
        assert_eq!(short.chars().count(), 64);
        assert!(short.starts_with(&full[..55]));
        assert_eq!(&short[55..56], "_");
        assert!(short[56..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(short, n.shorten(&full), "shortening is deterministic");
        assert_eq!(n.restore(&short), full);
        assert!(!n.is_empty());
    }

    #[test]
    fn names_sharing_a_long_prefix_stay_distinct() {
        let mut n = ToolNames::new();
        let a = format!("{}a", "p".repeat(70));
        let b = format!("{}b", "p".repeat(70));
        let (sa, sb) = (n.shorten(&a), n.shorten(&b));
        assert_ne!(sa, sb);
        assert_eq!(n.restore(&sa), a);
        assert_eq!(n.restore(&sb), b);
    }

    #[test]
    fn the_digest_is_the_first_four_bytes_of_sha256() {
        let mut n = ToolNames::new();
        let full = "q".repeat(65);
        let short = n.shorten(&full);
        let digest = Sha256::digest(full.as_bytes());
        let want: String = digest[..4].iter().map(|b| format!("{b:02x}")).collect();
        assert!(short.ends_with(&want));
    }

    #[test]
    fn multibyte_names_are_counted_in_characters() {
        let mut n = ToolNames::new();
        let full = "\u{e9}".repeat(70);
        let short = n.shorten(&full);
        assert_eq!(short.chars().count(), 64);
        assert_eq!(n.restore(&short), full);
    }

    #[test]
    fn unknown_names_restore_to_themselves() {
        assert_eq!(ToolNames::new().restore("whatever"), "whatever");
    }
}
