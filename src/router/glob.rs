//! Glob matching: `*` matches any run of characters (including none); everything else is literal.

/// Whether `text` matches `pattern`.
#[must_use]
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let mut star: Option<usize> = None;
    let mut mark = 0;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Whether `pattern` contains a wildcard.
#[must_use]
pub fn is_glob(pattern: &str) -> bool {
    pattern.contains('*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_patterns_match_exactly() {
        assert!(glob_match("claude-opus", "claude-opus"));
        assert!(!glob_match("claude-opus", "claude-opus-4"));
        assert!(!glob_match("claude-opus-4", "claude-opus"));
        assert!(glob_match("", ""));
        assert!(!glob_match("", "x"));
    }

    #[test]
    fn star_matches_any_run_including_none() {
        for (p, t, want) in [
            ("*", "", true),
            ("*", "anything", true),
            ("claude-*", "claude-", true),
            ("claude-*", "claude-opus-4", true),
            ("claude-*", "gpt-4", false),
            ("*-latest", "claude-opus-latest", true),
            ("*-latest", "claude-opus-latest-x", false),
            ("a*b*c", "abc", true),
            ("a*b*c", "aXXbYYc", true),
            ("a*b*c", "aXXbYY", false),
            ("**", "x", true),
            ("a*", "a", true),
            ("*a", "ba", true),
            ("*a", "bab", false),
            ("a*a", "a", false),
            ("claude-*-4*", "claude-opus-4-5", true),
        ] {
            assert_eq!(glob_match(p, t), want, "{p} vs {t}");
        }
    }

    #[test]
    fn matching_is_case_sensitive_and_unicode_aware() {
        assert!(!glob_match("Claude-*", "claude-x"));
        assert!(glob_match("caf\u{e9}-*", "caf\u{e9}-au-lait"));
        assert!(glob_match("*\u{1f600}", "hi\u{1f600}"));
    }

    #[test]
    fn globs_are_recognised() {
        assert!(is_glob("a*") && is_glob("*") && !is_glob("plain"));
    }

    #[test]
    fn pathological_patterns_stay_fast() {
        let text = "a".repeat(5000);
        assert!(!glob_match(&format!("{}b", "a*".repeat(50)), &text));
    }
}
