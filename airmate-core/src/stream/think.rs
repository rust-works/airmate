//! Inline `<think>…</think>` splitting.
//!
//! Some servers return reasoning inline in the content. The splitter acts only on a `<think>` that
//! opens the content after whitespace, and works across arbitrary chunk boundaries.

const OPEN: &str = "<think>";
const CLOSE: &str = "</think>";

/// A piece of content, classified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// Ordinary output text.
    Text(String),
    /// Reasoning text from inside the think tags.
    Reasoning(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    /// Before the first non-whitespace character: holds what might still become `<think>`.
    Start(String),
    /// Inside the tag: holds a tail that might become `</think>`.
    Inside(String),
    /// After the tag closed, or when the content did not open with one. The flag is set right
    /// after the closing tag, to drop the blank lines models put there.
    After { trim: bool },
}

/// An incremental `<think>` splitter.
#[derive(Debug, Clone)]
pub struct ThinkSplitter {
    state: State,
}

impl Default for ThinkSplitter {
    fn default() -> Self {
        Self {
            state: State::Start(String::new()),
        }
    }
}

impl ThinkSplitter {
    /// A splitter at the start of the content.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A splitter that passes everything through as text (`think_tags: false`).
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            state: State::After { trim: false },
        }
    }

    /// Feeds a chunk of content.
    pub fn push(&mut self, chunk: &str) -> Vec<Piece> {
        let mut out = Vec::new();
        let mut input = chunk.to_owned();
        loop {
            match &mut self.state {
                State::Start(held) => {
                    held.push_str(&input);
                    let trimmed = held.trim_start();
                    if trimmed.is_empty()
                        || (trimmed.len() < OPEN.len() && OPEN.starts_with(trimmed))
                    {
                        return out;
                    }
                    if let Some(rest) = trimmed.strip_prefix(OPEN) {
                        input = rest.to_owned();
                        self.state = State::Inside(String::new());
                    } else {
                        let all = std::mem::take(held);
                        self.state = State::After { trim: false };
                        out.push(Piece::Text(all));
                        return out;
                    }
                }
                State::Inside(tail) => {
                    tail.push_str(&input);
                    if let Some(i) = tail.find(CLOSE) {
                        let reasoning = tail[..i].to_owned();
                        input = tail[i + CLOSE.len()..].to_owned();
                        if !reasoning.is_empty() {
                            out.push(Piece::Reasoning(reasoning));
                        }
                        self.state = State::After { trim: true };
                        continue;
                    }
                    let keep = partial_suffix(tail, CLOSE);
                    let emit_len = tail.len() - keep;
                    if emit_len > 0 {
                        let emit = tail[..emit_len].to_owned();
                        *tail = tail[emit_len..].to_owned();
                        out.push(Piece::Reasoning(emit));
                    }
                    return out;
                }
                State::After { trim } => {
                    let text = if *trim {
                        input.trim_start().to_owned()
                    } else {
                        input.clone()
                    };
                    if !text.is_empty() {
                        *trim = false;
                        out.push(Piece::Text(text));
                    }
                    return out;
                }
            }
        }
    }

    /// Flushes anything held back at the end of the content.
    pub fn finish(&mut self) -> Vec<Piece> {
        match std::mem::replace(&mut self.state, State::After { trim: false }) {
            State::Start(held) if !held.is_empty() => vec![Piece::Text(held)],
            State::Inside(tail) if !tail.is_empty() => vec![Piece::Reasoning(tail)],
            _ => Vec::new(),
        }
    }
}

/// The length of the longest suffix of `s` that is a proper prefix of `pat`.
fn partial_suffix(s: &str, pat: &str) -> usize {
    (1..pat.len())
        .rev()
        .find(|&n| {
            n <= s.len() && s.is_char_boundary(s.len() - n) && pat.starts_with(&s[s.len() - n..])
        })
        .unwrap_or(0)
}

/// Splits a complete content string.
#[must_use]
pub fn split_all(content: &str, enabled: bool) -> Vec<Piece> {
    let mut s = if enabled {
        ThinkSplitter::new()
    } else {
        ThinkSplitter::disabled()
    };
    let mut out = s.push(content);
    out.extend(s.finish());
    merge(out)
}

/// Joins adjacent pieces of the same kind.
#[must_use]
pub fn merge(pieces: Vec<Piece>) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    for p in pieces {
        match (out.last_mut(), p) {
            (Some(Piece::Text(a)), Piece::Text(b))
            | (Some(Piece::Reasoning(a)), Piece::Reasoning(b)) => {
                a.push_str(&b);
            }
            (_, p) => out.push(p),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Piece {
        Piece::Text(s.into())
    }
    fn r(s: &str) -> Piece {
        Piece::Reasoning(s.into())
    }

    fn streamed(chunks: &[&str]) -> Vec<Piece> {
        let mut s = ThinkSplitter::new();
        let mut out = Vec::new();
        for c in chunks {
            out.extend(s.push(c));
        }
        out.extend(s.finish());
        merge(out)
    }

    #[test]
    fn a_leading_think_block_becomes_reasoning() {
        assert_eq!(
            split_all("<think>hmm</think>\n\nanswer", true),
            vec![r("hmm"), t("answer")]
        );
    }

    #[test]
    fn leading_whitespace_before_the_tag_is_allowed() {
        assert_eq!(
            split_all("  \n<think>x</think>y", true),
            vec![r("x"), t("y")]
        );
    }

    #[test]
    fn content_without_a_leading_tag_is_untouched() {
        assert_eq!(
            split_all("plain <think>not me</think>", true),
            vec![t("plain <think>not me</think>")]
        );
        assert_eq!(split_all("", true), Vec::<Piece>::new());
        assert_eq!(split_all(" ", true), vec![t(" ")]);
    }

    #[test]
    fn a_tag_that_is_not_closed_is_all_reasoning() {
        assert_eq!(
            split_all("<think>still going", true),
            vec![r("still going")]
        );
    }

    #[test]
    fn disabled_splitting_passes_everything_through() {
        assert_eq!(
            split_all("<think>x</think>y", false),
            vec![t("<think>x</think>y")]
        );
    }

    #[test]
    fn an_empty_think_block_yields_no_reasoning() {
        assert_eq!(split_all("<think></think>hi", true), vec![t("hi")]);
    }

    #[test]
    fn a_partial_open_tag_at_the_end_is_text() {
        assert_eq!(split_all("<thi", true), vec![t("<thi")]);
    }

    #[test]
    fn every_split_point_gives_the_same_result() {
        let content = "  <think>I wonder\nabout \u{e9}</think>\n\nThe answer is <think>42</think>.";
        let want = split_all(content, true);
        assert_eq!(
            want,
            vec![
                r("I wonder\nabout \u{e9}"),
                t("The answer is <think>42</think>.")
            ]
        );
        for i in 0..=content.len() {
            if !content.is_char_boundary(i) {
                continue;
            }
            let (a, b) = content.split_at(i);
            assert_eq!(streamed(&[a, b]), want, "split at {i}");
        }
    }

    #[test]
    fn char_at_a_time_matches_the_whole() {
        let content = "<think>ab</think>cd";
        let chunks: Vec<String> = content.chars().map(String::from).collect();
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        assert_eq!(streamed(&refs), split_all(content, true));
    }

    #[test]
    fn reasoning_streams_live_apart_from_a_possible_closing_tag() {
        let mut s = ThinkSplitter::new();
        assert_eq!(s.push("<think>abc"), vec![r("abc")]);
        assert_eq!(s.push("def</thi"), vec![r("def")]);
        assert_eq!(s.push("nk>ok"), vec![t("ok")]);
        assert!(s.finish().is_empty());
    }

    #[test]
    fn a_held_closing_prefix_that_never_completes_is_flushed() {
        let mut s = ThinkSplitter::new();
        assert_eq!(s.push("<think>x</th"), vec![r("x")]);
        assert_eq!(s.finish(), vec![r("</th")]);
        assert!(s.finish().is_empty());
    }

    #[test]
    fn merge_joins_neighbours_of_the_same_kind() {
        assert_eq!(
            merge(vec![t("a"), t("b"), r("c"), r("d"), t("e")]),
            vec![t("ab"), r("cd"), t("e")]
        );
    }
}
