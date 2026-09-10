//! The effect vocabulary.
//!
//! Samsara's whole model is this: **an agent run is deterministic apart from
//! the effects it performs**, so if we record every effect we can re-run the
//! agent and hand back the recorded answers.
//!
//! What counts as an effect is a judgement call, and getting it wrong is how
//! replay systems fail. Model calls and tool calls are obvious. Clock and
//! randomness are the ones people forget — and they are exactly the ones that
//! matter here, because retry backoff is computed from a jittered sleep. The
//! canonical agent bug (a tool times out, the retry duplicates a side effect)
//! is unreproducible unless you own the jitter.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::fault::Fault;
use crate::hash::Digest;

/// The kinds of external interaction an agent can have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectKind {
    /// A call to a language model.
    Model,
    /// A call to a tool the agent may invoke.
    Tool,
    /// A read of wall-clock time, in milliseconds since the epoch.
    Clock,
    /// A draw of randomness.
    Random,
}

impl EffectKind {
    /// Whether an effect of this kind can plausibly change the world.
    ///
    /// Only tools can. This drives the duplicate-side-effect invariant: two
    /// identical model calls are wasteful, two identical `delete_file` calls
    /// are an incident.
    pub fn is_effectful(&self) -> bool {
        matches!(self, EffectKind::Tool)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            EffectKind::Model => "model",
            EffectKind::Tool => "tool",
            EffectKind::Clock => "clock",
            EffectKind::Random => "random",
        }
    }
}

/// A request to perform an effect, as the agent issues it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EffectRequest {
    pub kind: EffectKind,
    /// Model id, tool name, or a fixed label for clock and random draws.
    pub name: String,
    /// The request payload. For a model call, the provider request body; for a
    /// tool call, its arguments.
    pub body: Value,
}

impl EffectRequest {
    pub fn model(name: impl Into<String>, body: Value) -> Self {
        EffectRequest {
            kind: EffectKind::Model,
            name: name.into(),
            body,
        }
    }

    pub fn tool(name: impl Into<String>, args: Value) -> Self {
        EffectRequest {
            kind: EffectKind::Tool,
            name: name.into(),
            body: args,
        }
    }

    pub fn clock() -> Self {
        EffectRequest {
            kind: EffectKind::Clock,
            name: "now_ms".into(),
            body: Value::Null,
        }
    }

    /// This effect's identity under `canon`.
    pub fn identity(&self, canon: &crate::canon::Canonicalizer) -> crate::hash::Digest {
        canon.effect_identity(self.kind.as_str(), &self.name, &self.body)
    }

    pub fn random() -> Self {
        EffectRequest {
            kind: EffectKind::Random,
            name: "next_f64".into(),
            body: Value::Null,
        }
    }
}

/// How an effect resolved.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    /// The effect returned a value.
    Ok { value: Value },
    /// The effect failed. `code` is machine-readable and is what fault
    /// injection synthesises; `message` is for humans.
    Err { code: String, message: String },
}

impl Outcome {
    pub fn ok(value: Value) -> Self {
        Outcome::Ok { value }
    }

    pub fn err(code: impl Into<String>, message: impl Into<String>) -> Self {
        Outcome::Err {
            code: code.into(),
            message: message.into(),
        }
    }

    pub fn is_ok(&self) -> bool {
        matches!(self, Outcome::Ok { .. })
    }

    pub fn value(&self) -> Option<&Value> {
        match self {
            Outcome::Ok { value } => Some(value),
            Outcome::Err { .. } => None,
        }
    }
}

/// One recorded effect: what was asked, what came back, and whether we
/// tampered with it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Position in the run, from zero. Also the fault-selector index.
    pub seq: u64,
    pub kind: EffectKind,
    pub name: String,
    /// Canonical identity of the request — what replay matches on.
    pub identity: Digest,
    /// CAS pointer to the full request body.
    pub request: Digest,
    /// CAS pointer to the serialised `Outcome`.
    pub outcome: Digest,
    /// Whether this outcome was synthesised rather than observed, and how.
    /// `None` on a clean recording; `Some` on a counterfactual branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault: Option<Fault>,
    /// The outcome the agent *would* have seen had we not injected `fault` —
    /// that is, what really happened.
    ///
    /// This field is what makes the duplicate-side-effect check exact rather
    /// than merely suspicious. When we inject a timeout over a call that
    /// actually succeeded, the agent believes the call failed and retries;
    /// only Samsara knows the side effect already landed. Without the shadow
    /// we could report "these two calls *might* both have taken effect".
    /// With it we can say they did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shadow: Option<Digest>,
    /// Monotone logical time. Samsara's clock effects return recorded wall
    /// time, but ordering is always by this, never by the wall clock.
    pub logical_time: u64,
}

impl Event {
    /// A one-line rendering for terminal output.
    pub fn summary(&self) -> String {
        let marker = if self.fault.is_some() { "!" } else { " " };
        format!(
            "{}{:>4} {:<7} {:<24} {}",
            marker,
            self.seq,
            self.kind.as_str(),
            truncate(&self.name, 24),
            self.identity.short()
        )
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}\u{2026}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tools_are_effectful() {
        assert!(EffectKind::Tool.is_effectful());
        assert!(!EffectKind::Model.is_effectful());
        assert!(!EffectKind::Clock.is_effectful());
        assert!(!EffectKind::Random.is_effectful());
    }

    #[test]
    fn outcomes_roundtrip_through_json() {
        for outcome in [
            Outcome::ok(serde_json::json!({"deleted": true})),
            Outcome::err("timeout", "no response in 30s"),
        ] {
            let bytes = serde_json::to_vec(&outcome).unwrap();
            let back: Outcome = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(outcome, back);
        }
    }

    #[test]
    fn summary_truncates_long_names_without_panicking_on_unicode() {
        let ev = Event {
            seq: 3,
            kind: EffectKind::Tool,
            name: "\u{1f600}".repeat(40),
            identity: Digest::of(b"x"),
            request: Digest::of(b"x"),
            outcome: Digest::of(b"x"),
            fault: None,
            shadow: None,
            logical_time: 3,
        };
        assert!(ev.summary().contains("\u{2026}"));
    }
}
