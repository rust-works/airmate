//! The prompt cache key.
//!
//! `cache_control` anywhere in a request sets a `prompt_cache_key` on upstreams that take one. The
//! router also prefers the same upstream for the same key, which keeps provider-side caches warm.

use sha2::{Digest, Sha256};

use crate::ir::IrRequest;

/// Hex of the first 16 bytes of SHA-256(model || system || tools).
#[must_use]
pub fn prompt_cache_key(req: &IrRequest) -> String {
    let mut h = Sha256::new();
    h.update(req.model.as_bytes());
    h.update([0]);
    for s in &req.system {
        h.update(s.text.as_bytes());
        h.update([0]);
    }
    for t in &req.tools {
        h.update(t.name.as_bytes());
        h.update([0]);
        h.update(t.description.as_deref().unwrap_or("").as_bytes());
        h.update([0]);
        h.update(t.input_schema.to_string().as_bytes());
        h.update([0]);
    }
    h.finalize()
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Protocol, SystemPart, ToolDef};
    use serde_json::json;

    fn req() -> IrRequest {
        let mut r = IrRequest::new("claude-x", Protocol::Anthropic);
        r.system.push(SystemPart {
            text: "be brief".into(),
            after_turns: 0,
            effort: None,
        });
        r.tools.push(ToolDef {
            name: "t".into(),
            description: None,
            input_schema: json!({}),
            strict: None,
        });
        r
    }

    #[test]
    fn the_key_is_32_hex_digits_and_stable() {
        let k = prompt_cache_key(&req());
        assert_eq!(k.len(), 32);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(k, prompt_cache_key(&req()));
    }

    #[test]
    fn model_system_and_tools_each_change_the_key() {
        let base = prompt_cache_key(&req());
        let mut m = req();
        m.model = "other".into();
        let mut s = req();
        s.system[0].text = "be verbose".into();
        let mut t = req();
        t.tools[0].name = "u".into();
        for changed in [m, s, t] {
            assert_ne!(base, prompt_cache_key(&changed));
        }
    }

    #[test]
    fn the_conversation_does_not_change_the_key() {
        let mut r = req();
        r.turns
            .push(crate::ir::Turn::User(vec![crate::ir::Part::Text(
                "hello".into(),
            )]));
        assert_eq!(prompt_cache_key(&req()), prompt_cache_key(&r));
    }
}
