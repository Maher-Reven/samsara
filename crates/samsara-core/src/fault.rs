//! Faults, fault schedules, and their generation from a seed.
//!
//! A *fault schedule* is the input Samsara searches over. It is a list of
//! `(event position, fault)` pairs, generated deterministically from a `u64`
//! seed, so a failing run is fully described by that one integer — which is
//! the entire pitch: `samsara repro 91238`.
//!
//! The representation is deliberately a flat `Vec`, because the shrinker
//! (see `shrink`) works by deleting subsets of it. Anything cleverer — a tree,
//! a state machine — would shrink worse, and shrinking is what turns a
//! fourteen-fault mess into a two-line bug report.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::event::{EffectKind, Outcome};

/// A single perturbation applied to one effect's outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Fault {
    /// The call never returns. The most productive fault by a wide margin:
    /// it is the one that provokes retries, and retries are where
    /// idempotency bugs live.
    Timeout,
    /// The call returns a provider error with this code.
    Error { code: String },
    /// The response is cut short after `keep` bytes — a dropped connection
    /// mid-stream. Against a JSON tool result this usually yields a parse
    /// failure; against a model response it can yield *valid but truncated*
    /// content, which is far nastier.
    Truncate { keep: usize },
    /// The transport delivers the same result twice.
    Duplicate,
    /// The result is syntactically invalid JSON.
    Malformed,
    /// The result is correct but arrives `ms` later, which can matter when
    /// the agent races it against a deadline.
    Delay { ms: u64 },
}

impl Fault {
    /// Short label for reports.
    pub fn label(&self) -> String {
        match self {
            Fault::Timeout => "timeout".into(),
            Fault::Error { code } => format!("error({code})"),
            Fault::Truncate { keep } => format!("truncate({keep}B)"),
            Fault::Duplicate => "duplicate".into(),
            Fault::Malformed => "malformed".into(),
            Fault::Delay { ms } => format!("delay({ms}ms)"),
        }
    }

    /// Apply this fault to an outcome, producing what the agent will actually
    /// observe.
    ///
    /// `Delay` deliberately leaves the value untouched: it perturbs timing,
    /// which is observable through clock effects, not content.
    pub fn apply(&self, observed: &Outcome) -> Outcome {
        match self {
            Fault::Timeout => Outcome::err("timeout", "samsara: injected timeout"),
            Fault::Error { code } => {
                Outcome::err(code.clone(), format!("samsara: injected error {code}"))
            }
            Fault::Delay { .. } | Fault::Duplicate => observed.clone(),
            Fault::Malformed => Outcome::Ok {
                value: Value::String("{\"truncated\": tru".into()),
            },
            Fault::Truncate { keep } => match observed {
                Outcome::Ok { value } => {
                    let text = serde_json::to_string(value).unwrap_or_default();
                    // Truncate on a character boundary; `keep` counts bytes but
                    // slicing mid-codepoint would panic.
                    let cut = floor_char_boundary(&text, *keep);
                    Outcome::Ok {
                        value: Value::String(text[..cut].to_string()),
                    }
                }
                other => other.clone(),
            },
        }
    }

    /// Which effect kinds this fault makes sense against.
    pub fn applies_to(&self, kind: EffectKind) -> bool {
        match kind {
            // Perturbing the clock or the RNG is not a fault, it is a
            // different run. Those effects are replayed verbatim, always.
            EffectKind::Clock | EffectKind::Random => false,
            EffectKind::Model | EffectKind::Tool => true,
        }
    }
}

/// Largest index `<= i` that lies on a UTF-8 character boundary.
fn floor_char_boundary(s: &str, i: usize) -> usize {
    let mut i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// One fault, bound to the effect it perturbs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultPoint {
    /// The `seq` of the event to perturb.
    pub seq: u64,
    pub fault: Fault,
}

/// An ordered set of perturbations to apply to a replay.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FaultSchedule {
    pub points: Vec<FaultPoint>,
}

impl FaultSchedule {
    pub fn empty() -> Self {
        FaultSchedule::default()
    }

    pub fn of(points: Vec<FaultPoint>) -> Self {
        FaultSchedule { points }
    }

    pub fn len(&self) -> usize {
        self.points.len()
    }

    pub fn is_empty(&self) -> bool {
        self.points.is_empty()
    }

    /// The fault to apply at `seq`, if any.
    ///
    /// If a schedule names the same `seq` twice — which shrinking can produce
    /// — the first wins, so the operation stays a pure function of the list.
    pub fn at(&self, seq: u64) -> Option<&Fault> {
        self.points.iter().find(|p| p.seq == seq).map(|p| &p.fault)
    }

    /// Generate a schedule for a run of `eligible` effects, from `seed`.
    ///
    /// `eligible` is the set of event positions that may be perturbed —
    /// clock and random effects are excluded by the caller, since faulting
    /// them would produce a different run rather than a faulted one.
    ///
    /// Density is capped: schedules that break everything prove nothing, and
    /// a run where every call fails is not a run worth debugging.
    pub fn generate(seed: u64, eligible: &[u64], max_faults: usize) -> Self {
        if eligible.is_empty() || max_faults == 0 {
            return FaultSchedule::empty();
        }
        let mut rng = ChaCha8Rng::seed_from_u64(seed);

        let cap = max_faults.min(eligible.len());
        let count = rng.gen_range(1..=cap);

        // Sample without replacement so one effect never carries two faults.
        let mut pool: Vec<u64> = eligible.to_vec();
        let mut points = Vec::with_capacity(count);
        for _ in 0..count {
            let idx = rng.gen_range(0..pool.len());
            let seq = pool.swap_remove(idx);
            points.push(FaultPoint {
                seq,
                fault: random_fault(&mut rng),
            });
        }

        // Ordering by seq keeps reports readable and makes equal schedules
        // compare equal regardless of draw order.
        points.sort_by_key(|p| p.seq);
        FaultSchedule { points }
    }

    /// Human-readable one-liner, e.g. `#3 timeout, #7 error(503)`.
    pub fn describe(&self) -> String {
        if self.points.is_empty() {
            return "(no faults)".into();
        }
        self.points
            .iter()
            .map(|p| format!("#{} {}", p.seq, p.fault.label()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Draw a fault, weighted toward the ones that find bugs.
///
/// `Timeout` is over-represented on purpose. In practice it is responsible for
/// most of the interesting failures, because it is the fault that makes an
/// agent retry, and retry paths are the least-tested code in any agent.
fn random_fault(rng: &mut ChaCha8Rng) -> Fault {
    match rng.gen_range(0..100) {
        0..=39 => Fault::Timeout,
        40..=59 => Fault::Error {
            code: ["500", "503", "429"][rng.gen_range(0..3)].to_string(),
        },
        60..=74 => Fault::Truncate {
            keep: rng.gen_range(0..64),
        },
        75..=84 => Fault::Duplicate,
        85..=94 => Fault::Malformed,
        _ => Fault::Delay {
            ms: rng.gen_range(100..30_000),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn generation_is_deterministic_in_the_seed() {
        let eligible: Vec<u64> = (0..20).collect();
        let a = FaultSchedule::generate(91238, &eligible, 5);
        let b = FaultSchedule::generate(91238, &eligible, 5);
        let c = FaultSchedule::generate(91239, &eligible, 5);

        assert_eq!(a, b, "same seed must give the same schedule");
        assert_ne!(a, c, "different seeds should differ");
    }

    #[test]
    fn generation_respects_bounds() {
        let eligible: Vec<u64> = (0..20).collect();
        for seed in 0..200u64 {
            let s = FaultSchedule::generate(seed, &eligible, 4);
            assert!(!s.is_empty() && s.len() <= 4);
            // No duplicate positions.
            let mut seqs: Vec<u64> = s.points.iter().map(|p| p.seq).collect();
            let before = seqs.len();
            seqs.sort_unstable();
            seqs.dedup();
            assert_eq!(before, seqs.len(), "sampling must be without replacement");
            // Every position is eligible, and the list is sorted.
            assert!(s.points.iter().all(|p| eligible.contains(&p.seq)));
            assert!(s.points.windows(2).all(|w| w[0].seq <= w[1].seq));
        }
    }

    #[test]
    fn empty_inputs_yield_empty_schedules() {
        assert!(FaultSchedule::generate(1, &[], 5).is_empty());
        assert!(FaultSchedule::generate(1, &[1, 2, 3], 0).is_empty());
    }

    #[test]
    fn timeout_replaces_success_with_an_error() {
        let observed = Outcome::ok(json!({"ok": true}));
        let faulted = Fault::Timeout.apply(&observed);
        assert!(!faulted.is_ok());
    }

    #[test]
    fn truncate_never_splits_a_codepoint() {
        let observed = Outcome::ok(json!("\u{1f600}\u{1f600}\u{1f600}"));
        // Every byte length from 0..20 must produce valid UTF-8, not a panic.
        for keep in 0..20 {
            let _ = Fault::Truncate { keep }.apply(&observed);
        }
    }

    #[test]
    fn clock_and_random_are_never_faultable() {
        for fault in [Fault::Timeout, Fault::Duplicate, Fault::Malformed] {
            assert!(!fault.applies_to(EffectKind::Clock));
            assert!(!fault.applies_to(EffectKind::Random));
            assert!(fault.applies_to(EffectKind::Tool));
        }
    }

    #[test]
    fn first_point_wins_when_a_seq_repeats() {
        let s = FaultSchedule::of(vec![
            FaultPoint {
                seq: 2,
                fault: Fault::Timeout,
            },
            FaultPoint {
                seq: 2,
                fault: Fault::Malformed,
            },
        ]);
        assert_eq!(s.at(2), Some(&Fault::Timeout));
    }
}
