//! Declaring what must never happen.
//!
//! Fault injection on its own answers "what breaks?" only if something is
//! watching. The tools in this space inject and then rely on your existing
//! test suite to notice, which means they find crashes and miss everything
//! an agent does wrong while staying up.
//!
//! Samsara watches. But what counts as wrong is not something a library can
//! know: `delete_file` twice is an incident, `search` twice is a waste, and
//! only the person who wrote the agent can say which of their tools is
//! which. So the properties are declared, in a file that lives beside the
//! agent and is read by every command.
//!
//! ```toml
//! [[invariant]]
//! type = "no_duplicate_effects"
//! tools = ["charge_card", "send_email"]
//! idempotency_key = "idempotency_key"
//!
//! [[invariant]]
//! type = "never_after_failure"
//! tool = "send_receipt"
//! after = "charge_card"
//! ```

use serde::{Deserialize, Serialize};

use crate::invariant::{
    Invariant, MaxCalls, NeverAfterFailure, NoDuplicateEffects, Requires, TerminatesWithin,
    TokenBudget,
};

/// One declared property.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum InvariantSpec {
    /// No side effect happens twice.
    NoDuplicateEffects {
        /// Tools to watch. Empty or absent means every tool, which is
        /// usually too broad to be useful — naturally idempotent reads will
        /// trip it and train you to ignore the result.
        #[serde(default)]
        tools: Vec<String>,
        /// Argument carrying an idempotency key; such calls are exempt.
        #[serde(default)]
        idempotency_key: Option<String>,
    },
    /// The run performs at most this many effects.
    TerminatesWithin { effects: usize },
    /// Model tokens stay under budget.
    TokenBudget { tokens: u64 },
    /// After `after` fails, `tool` must not be called.
    NeverAfterFailure { tool: String, after: String },
    /// If `tool` takes effect, `then` must too.
    Requires { tool: String, then: String },
    /// `tool` is called at most `max` times.
    MaxCalls { tool: String, max: usize },
}

impl InvariantSpec {
    /// Build the checker this describes.
    pub fn build(&self) -> Box<dyn Invariant> {
        match self {
            InvariantSpec::NoDuplicateEffects {
                tools,
                idempotency_key,
            } => {
                let mut check = if tools.is_empty() {
                    NoDuplicateEffects::all()
                } else {
                    NoDuplicateEffects::only(tools.clone())
                };
                if let Some(field) = idempotency_key {
                    check = check.exempting_idempotent(field.clone());
                }
                Box::new(check)
            }
            InvariantSpec::TerminatesWithin { effects } => Box::new(TerminatesWithin(*effects)),
            InvariantSpec::TokenBudget { tokens } => Box::new(TokenBudget(*tokens)),
            InvariantSpec::NeverAfterFailure { tool, after } => Box::new(NeverAfterFailure {
                tool: tool.clone(),
                after: after.clone(),
            }),
            InvariantSpec::Requires { tool, then } => Box::new(Requires {
                tool: tool.clone(),
                then: then.clone(),
            }),
            InvariantSpec::MaxCalls { tool, max } => Box::new(MaxCalls {
                tool: tool.clone(),
                max: *max,
            }),
        }
    }

    /// A one-line rendering, used in certificates so a reader can see the
    /// properties themselves and not merely their names.
    pub fn describe(&self) -> String {
        match self {
            InvariantSpec::NoDuplicateEffects {
                tools,
                idempotency_key,
            } => {
                let scope = if tools.is_empty() {
                    "every tool".into()
                } else {
                    tools.join(", ")
                };
                match idempotency_key {
                    Some(k) => format!("no duplicate effects on {scope} (exempting `{k}`)"),
                    None => format!("no duplicate effects on {scope}"),
                }
            }
            InvariantSpec::TerminatesWithin { effects } => {
                format!("terminates within {effects} effects")
            }
            InvariantSpec::TokenBudget { tokens } => format!("at most {tokens} tokens"),
            InvariantSpec::NeverAfterFailure { tool, after } => {
                format!("never `{tool}` after `{after}` fails")
            }
            InvariantSpec::Requires { tool, then } => format!("`{tool}` requires `{then}`"),
            InvariantSpec::MaxCalls { tool, max } => format!("at most {max} calls to `{tool}`"),
        }
    }
}

/// The properties an agent must hold.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
pub struct Config {
    /// Optional label, for reports.
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default, rename = "invariant")]
    pub invariants: Vec<InvariantSpec>,
}

impl Config {
    /// The properties that apply when nobody has declared any.
    ///
    /// Deliberately weak. A default that guessed which of your tools were
    /// effectful would be wrong for most agents, and wrong in the direction
    /// that produces noise — so the default only enforces what is true of
    /// every agent, and says nothing about side effects until you do.
    pub fn conservative_default() -> Self {
        Config {
            label: None,
            invariants: vec![InvariantSpec::TerminatesWithin { effects: 512 }],
        }
    }

    pub fn build(&self) -> Vec<Box<dyn Invariant>> {
        self.invariants.iter().map(InvariantSpec::build).collect()
    }

    /// Names, for a certificate.
    pub fn names(&self) -> Vec<String> {
        self.invariants.iter().map(|i| i.describe()).collect()
    }

    pub fn is_empty(&self) -> bool {
        self.invariants.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_spec_builds_a_checker_with_a_matching_name() {
        let specs = [
            InvariantSpec::NoDuplicateEffects {
                tools: vec![],
                idempotency_key: None,
            },
            InvariantSpec::TerminatesWithin { effects: 10 },
            InvariantSpec::TokenBudget { tokens: 10 },
            InvariantSpec::NeverAfterFailure {
                tool: "a".into(),
                after: "b".into(),
            },
            InvariantSpec::Requires {
                tool: "a".into(),
                then: "b".into(),
            },
            InvariantSpec::MaxCalls {
                tool: "a".into(),
                max: 1,
            },
        ];
        for spec in specs {
            let built = spec.build();
            assert!(!built.name().is_empty());
            assert!(!spec.describe().is_empty());
        }
    }

    #[test]
    fn specs_roundtrip_through_json() {
        let config = Config {
            label: Some("checkout".into()),
            invariants: vec![
                InvariantSpec::NoDuplicateEffects {
                    tools: vec!["charge_card".into()],
                    idempotency_key: Some("key".into()),
                },
                InvariantSpec::NeverAfterFailure {
                    tool: "send_receipt".into(),
                    after: "charge_card".into(),
                },
            ],
        };
        let text = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<Config>(&text).unwrap(), config);
    }

    #[test]
    fn an_unknown_field_is_rejected_rather_than_ignored() {
        // A typo in a config that silently does nothing is worse than an
        // error: you believe you are enforcing a property and you are not.
        let text = r#"{"type": "max_calls", "tool": "a", "maximum": 3}"#;
        assert!(serde_json::from_str::<InvariantSpec>(text).is_err());
    }

    #[test]
    fn an_unknown_invariant_type_is_rejected() {
        let text = r#"{"type": "no_such_property", "tool": "a"}"#;
        assert!(serde_json::from_str::<InvariantSpec>(text).is_err());
    }

    #[test]
    fn the_default_says_nothing_about_side_effects() {
        // Guessing which tools are effectful would be wrong for most agents,
        // and wrong in the direction that produces noise.
        let names = Config::conservative_default().names();
        assert_eq!(names.len(), 1);
        assert!(names[0].contains("terminates"));
    }
}
