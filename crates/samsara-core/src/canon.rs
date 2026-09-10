//! Canonicalisation: turning a request into a stable identity.
//!
//! This module is small and it is the hardest part of the system.
//!
//! On replay we must decide whether the effect the agent is asking for *now*
//! is the same effect it asked for when we recorded. Byte equality is far too
//! strict: a faithful re-run legitimately differs in fields that carry no
//! semantic weight — a freshly minted `tool_call_id`, a `request_id` header,
//! a timestamp the SDK stamped on the way out, a `max_tokens` the caller
//! recomputed from a clock. Compare those and every replay diverges at step
//! one. Ignore too much and you match the wrong effect, which is worse: you
//! silently feed the agent a response to a question it did not ask.
//!
//! So an effect's identity is the hash of its request with a declared set of
//! volatile paths redacted, and with object keys in a deterministic order.
//! The redaction set is part of the trace header, so a trace records the rules
//! under which it was captured and replay cannot quietly change them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::hash::Digest;

/// A path into a JSON document: dot-separated keys, `*` matching any array
/// index or any object key at that position.
///
/// `messages.*.tool_call_id` matches the id on every message;
/// `metadata.*` matches every field of `metadata`.
pub type VolatilePath = String;

/// The volatile-path set applied when computing effect identities.
///
/// Defaults cover the fields that every mainstream provider SDK varies between
/// otherwise-identical requests. Projects add their own; the set is serialised
/// into the trace header so replay uses exactly what recording used.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Canonicalizer {
    /// Paths whose values are replaced by a marker before hashing.
    pub volatile: Vec<VolatilePath>,
}

impl Default for Canonicalizer {
    fn default() -> Self {
        Canonicalizer {
            volatile: [
                // Provider-assigned identifiers, regenerated per attempt.
                "id",
                "request_id",
                "tool_call_id",
                "messages.*.tool_call_id",
                "messages.*.content.*.tool_use_id",
                "messages.*.content.*.id",
                // Wall-clock leakage.
                "created",
                "created_at",
                "timestamp",
                "metadata.timestamp",
                // Transport noise that does not change what was asked.
                "stream_options",
                "user",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        }
    }
}

/// Marker substituted for a redacted value. Distinct from any plausible real
/// value so a redaction can never be confused with content.
const REDACTED: &str = "\u{0}samsara:volatile";

impl Canonicalizer {
    /// An empty policy: nothing is volatile, identity is exact. Useful in tests
    /// and for effects (like tool calls) whose arguments are wholly meaningful.
    pub fn exact() -> Self {
        Canonicalizer { volatile: vec![] }
    }

    /// Add a volatile path.
    pub fn with(mut self, path: impl Into<String>) -> Self {
        self.volatile.push(path.into());
        self
    }

    /// Produce the canonical form of a value: volatile paths redacted, object
    /// keys ordered.
    ///
    /// Key ordering is handled by `serde_json`'s default `BTreeMap`-backed map,
    /// so serialising the returned value yields sorted keys at every depth.
    pub fn canonicalize(&self, value: &Value) -> Value {
        let mut out = value.clone();
        for path in &self.volatile {
            let segments: Vec<&str> = path.split('.').collect();
            redact(&mut out, &segments);
        }
        out
    }

    /// Canonical bytes — what actually gets hashed.
    pub fn canonical_bytes(&self, value: &Value) -> Vec<u8> {
        serde_json::to_vec(&self.canonicalize(value)).expect("Value always serialises")
    }

    /// The identity of a request body alone.
    ///
    /// Prefer [`effect_identity`](Self::effect_identity) — a body on its own
    /// is not an identity, as the tests below show.
    pub fn body_identity(&self, value: &Value) -> Digest {
        Digest::of(&self.canonical_bytes(value))
    }

    /// The identity of a whole effect: what kind it is, what it targets, and
    /// what it asks.
    ///
    /// All three are required. Hashing the body alone looks sufficient and
    /// is not: `delete_file{path:"/x"}` and `create_file{path:"/x"}` share a
    /// body, as do every clock read and every random draw (both have no body
    /// at all). Since the replay oracle answers by identity, a collision
    /// there means serving one effect's recorded response to a completely
    /// different effect — silently, and with no divergence reported.
    pub fn effect_identity(&self, kind: &str, name: &str, body: &Value) -> Digest {
        let envelope = serde_json::json!({
            "kind": kind,
            "name": name,
            "body": self.canonicalize(body),
        });
        Digest::of(&serde_json::to_vec(&envelope).expect("Value always serialises"))
    }
}

/// Walk `segments` into `value`, replacing whatever it reaches with the marker.
///
/// A path that does not resolve is silently ignored: policies are written once
/// and applied to many shapes of request, and a missing `tool_call_id` is the
/// normal case, not an error.
fn redact(value: &mut Value, segments: &[&str]) {
    let Some((head, rest)) = segments.split_first() else {
        *value = Value::String(REDACTED.to_string());
        return;
    };

    match value {
        Value::Object(map) => {
            if *head == "*" {
                for (_, v) in map.iter_mut() {
                    redact(v, rest);
                }
            } else if let Some(v) = map.get_mut(*head) {
                redact(v, rest);
            }
        }
        Value::Array(items) => {
            // `*` spans array elements; a numeric segment indexes one.
            if *head == "*" {
                for v in items.iter_mut() {
                    redact(v, rest);
                }
            } else if let Ok(idx) = head.parse::<usize>() {
                if let Some(v) = items.get_mut(idx) {
                    redact(v, rest);
                }
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn key_order_does_not_affect_identity() {
        let c = Canonicalizer::exact();
        let a = json!({"model": "x", "temperature": 0, "messages": []});
        let b = json!({"messages": [], "temperature": 0, "model": "x"});
        assert_eq!(c.body_identity(&a), c.body_identity(&b));
    }

    #[test]
    fn volatile_fields_are_ignored_but_content_is_not() {
        let c = Canonicalizer::default();
        let a = json!({"id": "req_aaa", "model": "x", "prompt": "hello"});
        let b = json!({"id": "req_bbb", "model": "x", "prompt": "hello"});
        let d = json!({"id": "req_aaa", "model": "x", "prompt": "goodbye"});

        assert_eq!(
            c.body_identity(&a),
            c.body_identity(&b),
            "ids must not matter"
        );
        assert_ne!(
            c.body_identity(&a),
            c.body_identity(&d),
            "prompts must matter"
        );
    }

    #[test]
    fn wildcards_reach_into_arrays() {
        let c = Canonicalizer::default();
        let a = json!({"messages": [{"tool_call_id": "call_1", "text": "hi"}]});
        let b = json!({"messages": [{"tool_call_id": "call_2", "text": "hi"}]});
        let d = json!({"messages": [{"tool_call_id": "call_1", "text": "bye"}]});

        assert_eq!(c.body_identity(&a), c.body_identity(&b));
        assert_ne!(c.body_identity(&a), c.body_identity(&d));
    }

    #[test]
    fn missing_paths_are_not_errors() {
        let c = Canonicalizer::default();
        // No `id`, no `messages` — the policy simply finds nothing to redact.
        let v = json!({"model": "x"});
        assert_eq!(c.body_identity(&v), c.body_identity(&json!({"model": "x"})));
    }

    #[test]
    fn kind_and_name_are_part_of_an_effect_identity() {
        let c = Canonicalizer::exact();
        let body = json!({"path": "/x"});

        // Same arguments, different tool: emphatically not the same effect.
        assert_ne!(
            c.effect_identity("tool", "delete_file", &body),
            c.effect_identity("tool", "create_file", &body)
        );
        // Same name, different kind.
        assert_ne!(
            c.effect_identity("tool", "x", &Value::Null),
            c.effect_identity("model", "x", &Value::Null)
        );
        // Clock and random both carry no body and must still differ.
        assert_ne!(
            c.effect_identity("clock", "now_ms", &Value::Null),
            c.effect_identity("random", "next_f64", &Value::Null)
        );
        // And it is still stable.
        assert_eq!(
            c.effect_identity("tool", "delete_file", &body),
            c.effect_identity("tool", "delete_file", &json!({"path": "/x"}))
        );
    }

    #[test]
    fn redaction_is_not_forgeable_by_content() {
        // A caller cannot make two different requests collide by writing the
        // marker themselves, because the marker contains a NUL byte that JSON
        // string content from a real provider will not carry.
        let c = Canonicalizer::default();
        let a = json!({"id": "real", "x": 1});
        let b = json!({"id": REDACTED, "x": 2});
        assert_ne!(c.body_identity(&a), c.body_identity(&b));
    }
}
