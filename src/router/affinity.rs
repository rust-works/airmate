//! Affinity: keeping a conversation on the target that served its previous turn.
//!
//! Requests carrying the same conversation fingerprint (a hash of the system prompt and the first
//! user turn) prefer the target that served the previous turn, unless it is unhealthy or
//! ineligible. This keeps provider-side prompt caches warm and keeps signed reasoning replayable.
//! Affinity is a soft preference held in a bounded in-memory LRU; losing it only costs a cache
//! miss.

use std::collections::{HashMap, VecDeque};

use serde_json::Value as Json;
use sha2::{Digest, Sha256};

use airmate_core::ir::Protocol;

/// A bounded least-recently-used map from conversation to target.
#[derive(Debug)]
pub struct Lru {
    cap: usize,
    map: HashMap<String, String>,
    order: VecDeque<String>,
}

impl Lru {
    /// An LRU holding at most `cap` conversations.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        Self {
            cap: cap.max(1),
            map: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.order.iter().position(|k| k == key) {
            self.order.remove(pos);
        }
        self.order.push_back(key.to_owned());
    }

    /// Remembers that `target` served `key`.
    pub fn put(&mut self, key: &str, target: &str) {
        self.map.insert(key.to_owned(), target.to_owned());
        self.touch(key);
        while self.map.len() > self.cap {
            if let Some(oldest) = self.order.pop_front() {
                self.map.remove(&oldest);
            }
        }
    }

    /// The target that last served `key`, refreshing its recency.
    pub fn get(&mut self, key: &str) -> Option<String> {
        let t = self.map.get(key).cloned()?;
        self.touch(key);
        Some(t)
    }

    /// How many conversations are remembered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether nothing is remembered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

fn text_of(v: &Json) -> String {
    match v {
        Json::String(s) => s.clone(),
        Json::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Json::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// A fingerprint of the conversation: the same for every turn of it, from the system prompt and
/// the first user turn. `None` when the body has no recognisable conversation.
#[must_use]
pub fn fingerprint(inbound: Protocol, body: &[u8]) -> Option<String> {
    let v: Json = serde_json::from_slice(body).ok()?;
    let first_user = |items: &Json| -> Option<String> {
        items
            .as_array()?
            .iter()
            .find(|m| m.get("role").and_then(Json::as_str) == Some("user"))
            .and_then(|m| m.get("content"))
            .map(text_of)
    };
    let (system, user) = match inbound {
        Protocol::Anthropic => (
            v.get("system").map(text_of).unwrap_or_default(),
            v.get("messages").and_then(first_user)?,
        ),
        Protocol::Chat => {
            let msgs = v.get("messages")?;
            let system = msgs
                .as_array()?
                .iter()
                .find(|m| {
                    matches!(
                        m.get("role").and_then(Json::as_str),
                        Some("system" | "developer")
                    )
                })
                .and_then(|m| m.get("content"))
                .map(text_of)
                .unwrap_or_default();
            (system, first_user(msgs)?)
        }
        _ => {
            let user = match v.get("input")? {
                Json::String(s) => s.clone(),
                other => first_user(other)?,
            };
            (
                v.get("instructions")
                    .and_then(Json::as_str)
                    .unwrap_or("")
                    .to_owned(),
                user,
            )
        }
    };
    let mut h = Sha256::new();
    h.update(
        v.get("model")
            .and_then(Json::as_str)
            .unwrap_or("")
            .as_bytes(),
    );
    h.update([0]);
    h.update(system.as_bytes());
    h.update([0]);
    h.update(user.as_bytes());
    Some(
        h.finalize()
            .iter()
            .take(16)
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_lru_remembers_and_forgets_the_least_recently_used() {
        let mut l = Lru::new(2);
        assert!(l.is_empty());
        l.put("a", "t1");
        l.put("b", "t2");
        assert_eq!(
            l.get("a").as_deref(),
            Some("t1"),
            "a is now the most recent"
        );
        l.put("c", "t3");
        assert_eq!(l.get("b"), None, "b was the least recently used");
        assert_eq!(
            (l.get("a").as_deref(), l.get("c").as_deref()),
            (Some("t1"), Some("t3"))
        );
        assert_eq!(l.len(), 2);
        l.put("a", "t9");
        assert_eq!(
            l.get("a").as_deref(),
            Some("t9"),
            "a later turn can move the conversation"
        );
        assert_eq!(l.len(), 2);
    }

    #[test]
    fn a_zero_capacity_still_holds_one() {
        let mut l = Lru::new(0);
        l.put("a", "t");
        assert_eq!(l.get("a").as_deref(), Some("t"));
    }

    #[test]
    fn every_turn_of_a_conversation_has_the_same_fingerprint() {
        let turn1 = json!({"model": "m", "system": "s", "messages": [{"role": "user", "content": "hello"}]});
        let turn2 = json!({"model": "m", "system": "s", "messages": [{"role": "user", "content": "hello"}, {"role": "assistant", "content": "hi"}, {"role": "user", "content": "more"}]});
        let a = fingerprint(Protocol::Anthropic, turn1.to_string().as_bytes());
        assert_eq!(
            a,
            fingerprint(Protocol::Anthropic, turn2.to_string().as_bytes())
        );
        assert_eq!(a.as_ref().unwrap().len(), 32);
        let other = json!({"model": "m", "system": "s", "messages": [{"role": "user", "content": "different"}]});
        assert_ne!(
            a,
            fingerprint(Protocol::Anthropic, other.to_string().as_bytes())
        );
        let other_system = json!({"model": "m", "system": "t", "messages": [{"role": "user", "content": "hello"}]});
        assert_ne!(
            a,
            fingerprint(Protocol::Anthropic, other_system.to_string().as_bytes())
        );
    }

    #[test]
    fn fingerprints_cover_block_content_chat_and_responses() {
        let blocks = json!({"model": "m", "system": [{"type": "text", "text": "s"}], "messages": [{"role": "user", "content": [{"type": "text", "text": "hello"}]}]});
        let plain = json!({"model": "m", "system": "s", "messages": [{"role": "user", "content": "hello"}]});
        assert_eq!(
            fingerprint(Protocol::Anthropic, blocks.to_string().as_bytes()),
            fingerprint(Protocol::Anthropic, plain.to_string().as_bytes())
        );
        let chat = json!({"model": "m", "messages": [{"role": "system", "content": "s"}, {"role": "user", "content": "hello"}]});
        assert!(fingerprint(Protocol::Chat, chat.to_string().as_bytes()).is_some());
        let r1 = json!({"model": "m", "instructions": "s", "input": "hello"});
        let r2 = json!({"model": "m", "instructions": "s", "input": [{"role": "user", "content": [{"type": "input_text", "text": "hello"}]}]});
        assert_eq!(
            fingerprint(Protocol::Responses, r1.to_string().as_bytes()),
            fingerprint(Protocol::Responses, r2.to_string().as_bytes())
        );
        assert_eq!(fingerprint(Protocol::Chat, b"nope"), None);
        assert_eq!(
            fingerprint(
                Protocol::Chat,
                br#"{"messages":[{"role":"assistant","content":"x"}]}"#
            ),
            None,
            "no user turn"
        );
    }
}
