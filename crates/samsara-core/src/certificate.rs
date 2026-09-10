//! A record of what was verified.
//!
//! A tool that says "all checks passed" is asking to be trusted. A
//! certificate says exactly *which* checks ran, how many there were, and
//! where the boundaries of the claim are — so a reader can decide for
//! themselves whether that is enough, and a reviewer can see when the
//! coverage silently shrank.
//!
//! It is deliberately free of timestamps, durations and machine names.
//! Everything in it is a function of the trace and the engine version, so
//! two runs produce byte-identical files. That is what makes it worth
//! committing: it belongs in a diff, where a pull request that quietly drops
//! the pair sweep or narrows the invariant set shows up as a changed line
//! rather than as nothing at all.

use serde::{Deserialize, Serialize};

use crate::explore::{Coverage, Interleavings};
use crate::hash::Digest;

/// The overall result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Nothing broke, within the stated bounds.
    Clean,
    /// Something broke. The certificate records what.
    Broken,
}

/// What was checked, and what happened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Certificate {
    /// Certificate schema version.
    pub version: u32,
    /// Engine that produced it. A different engine is a different claim.
    pub engine: String,
    /// The run this is about.
    pub trace: Digest,
    /// Human label from the trace header.
    pub label: String,
    /// Invariants that were enforced. Narrowing this list weakens every
    /// claim below it, which is exactly why it is recorded.
    pub invariants: Vec<String>,
    pub coverage: Coverage,
    pub interleavings: Vec<Interleavings>,
    pub verdict: Verdict,
    /// The claims, in plain English, as the tool would state them.
    pub claims: Vec<String>,
}

pub const CERTIFICATE_VERSION: u32 = 1;

impl Certificate {
    pub fn new(
        trace: Digest,
        label: String,
        invariants: Vec<String>,
        coverage: Coverage,
        interleavings: Vec<Interleavings>,
    ) -> Self {
        let broken =
            !coverage.is_clean() || interleavings.iter().any(|i| !i.is_order_independent());

        let mut claims = vec![coverage.claim()];
        claims.extend(interleavings.iter().map(Interleavings::claim));

        Certificate {
            version: CERTIFICATE_VERSION,
            engine: format!("samsara/{}", env!("CARGO_PKG_VERSION")),
            trace,
            label,
            invariants,
            coverage,
            interleavings,
            verdict: if broken {
                Verdict::Broken
            } else {
                Verdict::Clean
            },
            claims,
        }
    }

    /// Canonical JSON. Stable across runs, so it can be committed and
    /// diffed.
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("certificate serialises")
    }

    pub fn parse(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    /// Compare against a previously issued certificate.
    ///
    /// Reports differences in both directions. A regression is obvious; the
    /// subtler and more common case is coverage quietly shrinking while the
    /// verdict stays green, which is why counts are compared and not just
    /// the verdict.
    pub fn differences(&self, previous: &Certificate) -> Vec<String> {
        let mut out = Vec::new();

        if previous.trace != self.trace {
            out.push(format!(
                "different run: certificate is for trace {}, this is {}",
                previous.trace.short(),
                self.trace.short()
            ));
        }
        if previous.verdict != self.verdict {
            out.push(format!(
                "verdict changed: {:?} \u{2192} {:?}",
                previous.verdict, self.verdict
            ));
        }
        if previous.invariants != self.invariants {
            out.push(format!(
                "invariants changed: {:?} \u{2192} {:?}",
                previous.invariants, self.invariants
            ));
        }

        let (a, b) = (&previous.coverage, &self.coverage);
        if a.singles_checked != b.singles_checked {
            out.push(format!(
                "single-fault coverage {} \u{2192} {}",
                a.singles_checked, b.singles_checked
            ));
        }
        if a.pairs_checked != b.pairs_checked {
            out.push(format!(
                "pair coverage {} \u{2192} {}",
                a.pairs_checked, b.pairs_checked
            ));
        }
        if a.singles_exhaustive && !b.singles_exhaustive {
            out.push("single-fault sweep is no longer exhaustive".into());
        }
        if a.failures.len() != b.failures.len() {
            out.push(format!(
                "failing schedules {} \u{2192} {}",
                a.failures.len(),
                b.failures.len()
            ));
        }

        for (before, after) in previous.interleavings.iter().zip(&self.interleavings) {
            if before.divergent.len() != after.divergent.len() {
                out.push(format!(
                    "batch #{}: divergent orderings {} \u{2192} {}",
                    after.batch,
                    before.divergent.len(),
                    after.divergent.len()
                ));
            }
            if before.exhaustive && !after.exhaustive {
                out.push(format!(
                    "batch #{}: no longer checked exhaustively",
                    after.batch
                ));
            }
        }
        if previous.interleavings.len() != self.interleavings.len() {
            out.push(format!(
                "concurrent batches {} \u{2192} {}",
                previous.interleavings.len(),
                self.interleavings.len()
            ));
        }

        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explore::Coverage;

    fn coverage(singles: usize, failures: usize) -> Coverage {
        Coverage {
            positions: 3,
            kinds: 5,
            singles_checked: singles,
            singles_exhaustive: true,
            pairs_checked: 0,
            pairs_exhaustive: false,
            replays: singles,
            failures: (0..failures)
                .map(|_| crate::explore::Case {
                    schedule: crate::fault::FaultSchedule::empty(),
                    description: "x".into(),
                    violations: vec![],
                })
                .collect(),
        }
    }

    fn cert(singles: usize, failures: usize) -> Certificate {
        Certificate::new(
            Digest::of(b"trace"),
            "run".into(),
            vec!["no_duplicate_effects".into()],
            coverage(singles, failures),
            vec![],
        )
    }

    #[test]
    fn a_clean_sweep_yields_a_clean_verdict() {
        assert_eq!(cert(15, 0).verdict, Verdict::Clean);
        assert_eq!(cert(15, 1).verdict, Verdict::Broken);
    }

    #[test]
    fn certificates_are_stable_across_runs() {
        // No timestamps, no durations, no hostnames: the same inputs must
        // give the same bytes or the file is useless in a diff.
        assert_eq!(cert(15, 0).to_json(), cert(15, 0).to_json());
    }

    #[test]
    fn it_roundtrips() {
        let c = cert(15, 1);
        assert_eq!(Certificate::parse(&c.to_json()).unwrap(), c);
    }

    #[test]
    fn identical_certificates_differ_in_nothing() {
        assert!(cert(15, 0).differences(&cert(15, 0)).is_empty());
    }

    #[test]
    fn a_new_failure_is_reported() {
        let diff = cert(15, 1).differences(&cert(15, 0));
        assert!(
            diff.iter().any(|d| d.contains("verdict changed")),
            "{diff:?}"
        );
    }

    #[test]
    fn coverage_shrinking_is_reported_even_when_the_verdict_stays_green() {
        // The quiet regression: still passing, but checking less. A tool
        // that only compared verdicts would call this an improvement.
        let diff = cert(4, 0).differences(&cert(15, 0));
        assert_eq!(cert(4, 0).verdict, Verdict::Clean);
        assert!(
            diff.iter().any(|d| d.contains("single-fault coverage 15")),
            "{diff:?}"
        );
    }

    #[test]
    fn a_narrowed_invariant_set_is_reported() {
        let mut weakened = cert(15, 0);
        weakened.invariants.clear();
        let diff = weakened.differences(&cert(15, 0));
        assert!(
            diff.iter().any(|d| d.contains("invariants changed")),
            "{diff:?}"
        );
    }
}
