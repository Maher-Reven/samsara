//! Invariants: the properties an agent must hold under *every* fault schedule.
//!
//! Fault injection alone only tells you the agent crashed. Invariants tell you
//! it did something worse — that it stayed up and quietly did the wrong thing.
//! The duplicate-side-effect invariant is the one this project was built
//! around, and it is the one that catches the bug nobody finds in review.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::cas::Cas;
use crate::event::{EffectKind, Outcome};
use crate::hash::Digest;
use crate::trace::Trace;

/// A breach of an invariant, located in the run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Violation {
    /// Which invariant was broken.
    pub invariant: String,
    /// Where, if the breach has a single location.
    pub at_seq: Option<u64>,
    /// What went wrong, phrased for a bug report.
    pub detail: String,
}

impl Violation {
    pub fn report(&self) -> String {
        match self.at_seq {
            Some(seq) => format!("[{}] at effect #{}: {}", self.invariant, seq, self.detail),
            None => format!("[{}] {}", self.invariant, self.detail),
        }
    }
}

/// A property checked against a completed run.
pub trait Invariant {
    fn name(&self) -> &str;
    /// Return every breach found. An empty vector means the run is clean.
    fn check(&self, trace: &Trace, cas: &dyn Cas) -> Vec<Violation>;
}

/// Read an event's outcome out of the store.
fn outcome_of(digest: &Digest, cas: &dyn Cas) -> Option<Outcome> {
    cas.get(digest)
        .ok()
        .flatten()
        .and_then(|b| serde_json::from_slice::<Outcome>(&b).ok())
}

// ---------------------------------------------------------------------------

/// Whether a call's side effect actually took place.
///
/// The three-way answer is the point. A timeout is not a failure — it is an
/// *absence of information*. The request may have been received, executed,
/// and its response lost on the way back. Treating a timeout as "did not
/// happen" is precisely the mistake the agent under test makes, and an
/// invariant that made the same mistake would never catch it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Landing {
    /// The side effect definitely happened.
    Yes,
    /// It may have happened; the response was lost, not refused.
    Maybe,
    /// It definitely did not — the call was refused before doing anything.
    No,
}

/// Error codes that leave the outcome genuinely unknown.
///
/// A `400` means the server understood and rejected: nothing happened. A
/// `503` or a dropped connection means we have no idea.
const AMBIGUOUS_CODES: &[&str] = &[
    "timeout",
    "connection_reset",
    "connection_refused",
    "unknown",
    "502",
    "503",
    "504",
];

/// Decide whether an event's side effect landed.
fn landing(event: &crate::event::Event, cas: &dyn Cas) -> Landing {
    // If a fault was injected here, we hold the truth in the shadow: the
    // outcome the agent was prevented from seeing.
    if event.fault.is_some() {
        if let Some(shadow) = &event.shadow {
            return match outcome_of(shadow, cas) {
                Some(o) if o.is_ok() => Landing::Yes,
                Some(_) => Landing::No,
                None => Landing::Maybe,
            };
        }
        return Landing::Maybe;
    }

    match outcome_of(&event.outcome, cas) {
        Some(Outcome::Ok { .. }) => Landing::Yes,
        Some(Outcome::Err { code, .. }) => {
            if AMBIGUOUS_CODES.iter().any(|c| code.eq_ignore_ascii_case(c)) {
                Landing::Maybe
            } else {
                Landing::No
            }
        }
        None => Landing::Maybe,
    }
}

/// **No side effect happens twice.**
///
/// The flagship. A tool times out, the agent retries, and the first call had
/// actually succeeded — so the file is deleted twice, the customer is charged
/// twice, the email goes out twice. In production this surfaces once in a few
/// hundred runs and is never reproducible. Here it is a failed assertion.
///
/// Two subtleties separate this from a naive "same call twice" check:
///
/// - **Timeouts count as possible successes** (see [`Landing`]). A retry
///   after a timeout is exactly the dangerous case, and it is the one where
///   the first call *looks* like it failed.
/// - **Declared-idempotent calls are exempt.** The industry fix for this bug
///   is an idempotency key that is stable across retries; a tool holding such
///   a key is contractually obliged to deduplicate, so retrying it is correct
///   and must not be flagged. Set [`idempotency_field`] to the argument your
///   tools use.
///
/// [`idempotency_field`]: NoDuplicateEffects::idempotency_field
#[derive(Debug, Clone, Default)]
pub struct NoDuplicateEffects {
    /// Restrict to these tool names. Empty means every tool.
    ///
    /// Worth setting: `search`, `read_file` and friends are naturally
    /// idempotent, so leaving them in produces noise that trains people to
    /// ignore the check.
    pub tools: Vec<String>,
    /// Argument name carrying an idempotency key. Calls that supply one are
    /// exempt.
    pub idempotency_field: Option<String>,
}

impl NoDuplicateEffects {
    /// Watch every tool.
    pub fn all() -> Self {
        Self::default()
    }

    /// Watch only the named tools — the ones that actually mutate something.
    pub fn only(tools: impl IntoIterator<Item = impl Into<String>>) -> Self {
        NoDuplicateEffects {
            tools: tools.into_iter().map(Into::into).collect(),
            idempotency_field: None,
        }
    }

    /// Exempt calls that carry an idempotency key in this argument.
    pub fn exempting_idempotent(mut self, field: impl Into<String>) -> Self {
        self.idempotency_field = Some(field.into());
        self
    }

    fn watches(&self, name: &str) -> bool {
        self.tools.is_empty() || self.tools.iter().any(|t| t == name)
    }

    /// Whether this call declares an idempotency key.
    fn is_declared_idempotent(&self, event: &crate::event::Event, cas: &dyn Cas) -> bool {
        let Some(field) = &self.idempotency_field else {
            return false;
        };
        let Some(bytes) = cas.get(&event.request).ok().flatten() else {
            return false;
        };
        let Ok(request) = serde_json::from_slice::<crate::event::EffectRequest>(&bytes) else {
            return false;
        };
        request
            .body
            .get(field)
            .map(|v| !v.is_null())
            .unwrap_or(false)
    }
}

impl Invariant for NoDuplicateEffects {
    fn name(&self) -> &str {
        "no_duplicate_effects"
    }

    fn check(&self, trace: &Trace, cas: &dyn Cas) -> Vec<Violation> {
        // identity -> (seq of first landing, whether that landing was certain)
        let mut landed: HashMap<Digest, (u64, Landing)> = HashMap::new();
        let mut violations = Vec::new();

        for event in &trace.events {
            if !event.kind.is_effectful() || !self.watches(&event.name) {
                continue;
            }
            if self.is_declared_idempotent(event, cas) {
                continue;
            }

            let this = landing(event, cas);
            if this == Landing::No {
                continue;
            }

            match landed.get(&event.identity) {
                Some((first, prior)) => {
                    let certain = *prior == Landing::Yes && this == Landing::Yes;
                    violations.push(Violation {
                        invariant: "no_duplicate_effects".into(),
                        at_seq: Some(event.seq),
                        detail: if certain {
                            format!(
                                "`{}` took effect at #{} and again at #{} with identical \
                                 arguments ({}) — the side effect happened twice",
                                event.name,
                                first,
                                event.seq,
                                event.identity.short()
                            )
                        } else {
                            format!(
                                "`{}` at #{} returned an ambiguous failure, so it may already \
                                 have taken effect; #{} repeated it with identical arguments \
                                 ({}). Supply an idempotency key or reconcile before retrying.",
                                event.name,
                                first,
                                event.seq,
                                event.identity.short()
                            )
                        },
                    });
                }
                None => {
                    landed.insert(event.identity.clone(), (event.seq, this));
                }
            }
        }
        violations
    }
}

// ---------------------------------------------------------------------------

/// **The agent finishes within a bounded number of effects.**
///
/// Catches the retry storm: one flaky tool, and the agent loops until someone
/// notices the bill.
#[derive(Debug, Clone)]
pub struct TerminatesWithin(pub usize);

impl Invariant for TerminatesWithin {
    fn name(&self) -> &str {
        "terminates_within"
    }

    fn check(&self, trace: &Trace, _cas: &dyn Cas) -> Vec<Violation> {
        if trace.len() > self.0 {
            vec![Violation {
                invariant: "terminates_within".into(),
                at_seq: Some(self.0 as u64),
                detail: format!(
                    "run performed {} effects, budget is {}",
                    trace.len(),
                    self.0
                ),
            }]
        } else {
            vec![]
        }
    }
}

// ---------------------------------------------------------------------------

/// **Total model tokens stay under budget.**
///
/// Reads `usage.input_tokens` / `usage.output_tokens` from model outcomes,
/// which is the shape both major providers emit. Responses without a usage
/// block contribute nothing rather than being guessed at — an invariant that
/// invents numbers is worse than no invariant.
#[derive(Debug, Clone)]
pub struct TokenBudget(pub u64);

impl Invariant for TokenBudget {
    fn name(&self) -> &str {
        "token_budget"
    }

    fn check(&self, trace: &Trace, cas: &dyn Cas) -> Vec<Violation> {
        let mut total = 0u64;
        let mut breached_at = None;

        for event in &trace.events {
            if event.kind != EffectKind::Model {
                continue;
            }
            let Some(Outcome::Ok { value }) = outcome_of(&event.outcome, cas) else {
                continue;
            };
            let usage = value.get("usage");
            let count = |k: &str| -> u64 {
                usage
                    .and_then(|u| u.get(k))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            };
            total += count("input_tokens") + count("output_tokens");

            if total > self.0 && breached_at.is_none() {
                breached_at = Some(event.seq);
            }
        }

        match breached_at {
            Some(seq) => vec![Violation {
                invariant: "token_budget".into(),
                at_seq: Some(seq),
                detail: format!("run consumed {total} tokens, budget is {}", self.0),
            }],
            None => vec![],
        }
    }
}

// ---------------------------------------------------------------------------

/// Check a run against a set of invariants.
pub fn check_all(
    trace: &Trace,
    cas: &dyn Cas,
    invariants: &[Box<dyn Invariant>],
) -> Vec<Violation> {
    invariants
        .iter()
        .flat_map(|i| i.check(trace, cas))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cas::MemCas;
    use crate::event::{EffectRequest, Event};
    use crate::fault::Fault;
    use crate::trace::TraceHeader;
    use serde_json::json;

    struct Row {
        kind: EffectKind,
        name: &'static str,
        args: serde_json::Value,
        outcome: Outcome,
        fault: Option<Fault>,
        shadow: Option<Outcome>,
    }

    /// A plain effect with no tampering.
    fn row(kind: EffectKind, name: &'static str, args: serde_json::Value, outcome: Outcome) -> Row {
        Row {
            kind,
            name,
            args,
            outcome,
            fault: None,
            shadow: None,
        }
    }

    /// An effect whose real outcome was suppressed by an injected fault —
    /// what a counterfactual branch contains.
    fn faulted(name: &'static str, args: serde_json::Value, real: Outcome, f: Fault) -> Row {
        Row {
            kind: EffectKind::Tool,
            name,
            outcome: f.apply(&real),
            args,
            fault: Some(f),
            shadow: Some(real),
        }
    }

    fn build(rows: Vec<Row>) -> (Trace, MemCas) {
        let mut cas = MemCas::new();
        let canon = crate::canon::Canonicalizer::exact();
        let mut events = Vec::new();
        for (seq, r) in rows.into_iter().enumerate() {
            let request = EffectRequest {
                kind: r.kind,
                name: r.name.into(),
                body: r.args.clone(),
            };
            let request_digest = cas.put(&serde_json::to_vec(&request).unwrap()).unwrap();
            let outcome_digest = cas.put(&serde_json::to_vec(&r.outcome).unwrap()).unwrap();
            let shadow = r
                .shadow
                .map(|o| cas.put(&serde_json::to_vec(&o).unwrap()).unwrap());
            events.push(Event {
                seq: seq as u64,
                kind: r.kind,
                name: r.name.into(),
                identity: request.identity(&canon),
                request: request_digest,
                outcome: outcome_digest,
                fault: r.fault,
                shadow,
                logical_time: seq as u64,
            });
        }
        (
            Trace {
                header: TraceHeader::default(),
                events,
            },
            cas,
        )
    }

    fn ok() -> Outcome {
        Outcome::ok(json!({"ok": true}))
    }

    // -- landing semantics ------------------------------------------------

    #[test]
    fn a_definite_rejection_did_not_land() {
        let (t, cas) = build(vec![row(
            EffectKind::Tool,
            "delete_file",
            json!({"path": "/x"}),
            Outcome::err("400", "bad path"),
        )]);
        assert_eq!(landing(&t.events[0], &cas), Landing::No);
    }

    #[test]
    fn a_timeout_might_have_landed() {
        let (t, cas) = build(vec![row(
            EffectKind::Tool,
            "delete_file",
            json!({"path": "/x"}),
            Outcome::err("timeout", "no response"),
        )]);
        assert_eq!(landing(&t.events[0], &cas), Landing::Maybe);
    }

    #[test]
    fn an_injected_timeout_over_a_success_definitely_landed() {
        // This is the whole reason the shadow field exists.
        let (t, cas) = build(vec![faulted(
            "delete_file",
            json!({"path": "/x"}),
            ok(),
            Fault::Timeout,
        )]);
        assert_eq!(landing(&t.events[0], &cas), Landing::Yes);
    }

    // -- the flagship -----------------------------------------------------

    #[test]
    fn the_flagship_bug_is_caught_with_certainty() {
        // Call one is timed out by injection but really succeeded; the agent
        // believes it failed and retries; the retry succeeds for real.
        let (t, cas) = build(vec![
            faulted("delete_file", json!({"path": "/x"}), ok(), Fault::Timeout),
            row(EffectKind::Tool, "delete_file", json!({"path": "/x"}), ok()),
        ]);
        let v = NoDuplicateEffects::all().check(&t, &cas);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].at_seq, Some(1));
        assert!(v[0].detail.contains("happened twice"), "{}", v[0].detail);
    }

    #[test]
    fn a_real_timeout_yields_the_hedged_wording() {
        // Without a shadow we cannot know, and must not claim to.
        let (t, cas) = build(vec![
            row(
                EffectKind::Tool,
                "charge",
                json!({"amt": 5}),
                Outcome::err("503", "gateway"),
            ),
            row(EffectKind::Tool, "charge", json!({"amt": 5}), ok()),
        ]);
        let v = NoDuplicateEffects::all().check(&t, &cas);
        assert_eq!(v.len(), 1);
        assert!(v[0].detail.contains("may already"), "{}", v[0].detail);
    }

    #[test]
    fn a_retry_after_a_definite_rejection_is_correct_behaviour() {
        let (t, cas) = build(vec![
            row(
                EffectKind::Tool,
                "delete_file",
                json!({"path": "/x"}),
                Outcome::err("400", "bad"),
            ),
            row(EffectKind::Tool, "delete_file", json!({"path": "/x"}), ok()),
        ]);
        assert!(NoDuplicateEffects::all().check(&t, &cas).is_empty());
    }

    #[test]
    fn an_idempotency_key_exempts_the_retry() {
        // The industry fix: a key stable across attempts. Retrying is now
        // correct, and flagging it would be a false positive.
        let args = json!({"path": "/x", "idempotency_key": "task-1"});
        let (t, cas) = build(vec![
            faulted("delete_file", args.clone(), ok(), Fault::Timeout),
            row(EffectKind::Tool, "delete_file", args, ok()),
        ]);

        assert_eq!(NoDuplicateEffects::all().check(&t, &cas).len(), 1);
        assert!(NoDuplicateEffects::all()
            .exempting_idempotent("idempotency_key")
            .check(&t, &cas)
            .is_empty());
    }

    #[test]
    fn a_null_idempotency_key_does_not_count_as_one() {
        let args = json!({"path": "/x", "idempotency_key": null});
        let (t, cas) = build(vec![
            row(EffectKind::Tool, "delete_file", args.clone(), ok()),
            row(EffectKind::Tool, "delete_file", args, ok()),
        ]);
        assert_eq!(
            NoDuplicateEffects::all()
                .exempting_idempotent("idempotency_key")
                .check(&t, &cas)
                .len(),
            1
        );
    }

    #[test]
    fn different_arguments_are_different_effects() {
        let (t, cas) = build(vec![
            row(EffectKind::Tool, "delete_file", json!({"path": "/x"}), ok()),
            row(EffectKind::Tool, "delete_file", json!({"path": "/y"}), ok()),
        ]);
        assert!(NoDuplicateEffects::all().check(&t, &cas).is_empty());
    }

    #[test]
    fn model_calls_are_never_duplicates_however_repeated() {
        let (t, cas) = build(vec![
            row(EffectKind::Model, "sonnet", json!({"p": 1}), ok()),
            row(EffectKind::Model, "sonnet", json!({"p": 1}), ok()),
        ]);
        assert!(NoDuplicateEffects::all().check(&t, &cas).is_empty());
    }

    #[test]
    fn the_watch_list_filters_idempotent_tools() {
        let (t, cas) = build(vec![
            row(EffectKind::Tool, "read_file", json!({"path": "/x"}), ok()),
            row(EffectKind::Tool, "read_file", json!({"path": "/x"}), ok()),
        ]);
        assert!(NoDuplicateEffects::only(["delete_file"])
            .check(&t, &cas)
            .is_empty());
        assert_eq!(NoDuplicateEffects::all().check(&t, &cas).len(), 1);
    }

    // -- the other two ----------------------------------------------------

    #[test]
    fn termination_budget_is_inclusive() {
        let (t, cas) = build(vec![
            row(EffectKind::Tool, "t", json!(1), ok()),
            row(EffectKind::Tool, "t", json!(2), ok()),
        ]);
        assert!(
            TerminatesWithin(2).check(&t, &cas).is_empty(),
            "exactly at budget is fine"
        );
        assert_eq!(TerminatesWithin(1).check(&t, &cas).len(), 1);
    }

    #[test]
    fn token_budget_sums_usage_and_ignores_responses_without_it() {
        let usage =
            |i: u64, o: u64| Outcome::ok(json!({"usage": {"input_tokens": i, "output_tokens": o}}));
        let (t, cas) = build(vec![
            row(EffectKind::Model, "m", json!(1), usage(100, 50)),
            row(
                EffectKind::Model,
                "m",
                json!(2),
                Outcome::ok(json!({"text": "no usage"})),
            ),
            row(EffectKind::Model, "m", json!(3), usage(200, 25)),
        ]);
        assert!(
            TokenBudget(400).check(&t, &cas).is_empty(),
            "375 is under 400"
        );
        let v = TokenBudget(300).check(&t, &cas);
        assert_eq!(v.len(), 1);
        assert_eq!(
            v[0].at_seq,
            Some(2),
            "reports where the budget was first breached"
        );
    }

    #[test]
    fn check_all_aggregates() {
        let (t, cas) = build(vec![
            row(EffectKind::Tool, "delete_file", json!({"path": "/x"}), ok()),
            row(EffectKind::Tool, "delete_file", json!({"path": "/x"}), ok()),
        ]);
        let invariants: Vec<Box<dyn Invariant>> = vec![
            Box::new(NoDuplicateEffects::all()),
            Box::new(TerminatesWithin(1)),
        ];
        assert_eq!(check_all(&t, &cas, &invariants).len(), 2);
    }
}
