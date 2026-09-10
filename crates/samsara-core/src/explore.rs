//! Searching the space of fault schedules.
//!
//! Given a recording and a set of invariants, the question "is this agent
//! robust?" becomes a search: enumerate seeds, generate a schedule from each,
//! replay under it, and check the invariants. Because a schedule is a pure
//! function of its seed, a failure is reported as one integer and reproduces
//! anywhere.
//!
//! Then [`minimise`] throws away the parts of the schedule that were not to
//! blame, which is the difference between a bug report someone reads and a
//! bug report someone closes.

use crate::cas::Cas;
use crate::fault::FaultSchedule;
use crate::invariant::{Invariant, Violation};
use crate::replay::{Mode, Replayer};
use crate::shrink::{shrink, Shrunk};
use crate::trace::Trace;

/// A schedule that breaks the agent.
#[derive(Clone, Debug)]
pub struct Finding {
    /// The seed that generated it. This is the whole bug report.
    pub seed: u64,
    pub schedule: FaultSchedule,
    pub violations: Vec<Violation>,
}

impl Finding {
    pub fn report(&self) -> String {
        let mut s = format!(
            "seed {} broke {} invariant(s)\n",
            self.seed,
            self.violations.len()
        );
        s.push_str(&format!("  schedule: {}\n", self.schedule.describe()));
        for v in &self.violations {
            s.push_str(&format!("  {}\n", v.report()));
        }
        s
    }
}

/// Replay `trace` under `schedule`, driving the agent with `drive`, and
/// return whichever invariants it broke.
pub fn evaluate<C, F>(
    trace: &Trace,
    cas: &mut C,
    schedule: FaultSchedule,
    invariants: &[Box<dyn Invariant>],
    mut drive: F,
) -> Vec<Violation>
where
    C: Cas,
    F: FnMut(&mut Replayer<'_, C>),
{
    let branch = {
        let mut replayer = Replayer::new(trace, cas, Mode::Counterfactual { schedule })
            .expect("replayer construction reads only the store");
        drive(&mut replayer);
        replayer.finish("counterfactual").branch
    };
    crate::invariant::check_all(&branch, cas, invariants)
}

/// Try each seed in turn, returning the first that breaks an invariant.
///
/// Stops at the first finding rather than collecting all of them: a single
/// reproducible failure is worth more than a list, because the second bug is
/// usually the first bug wearing different clothes.
pub fn search<C, F>(
    trace: &Trace,
    cas: &mut C,
    seeds: impl IntoIterator<Item = u64>,
    max_faults: usize,
    invariants: &[Box<dyn Invariant>],
    mut drive: F,
) -> Option<Finding>
where
    C: Cas,
    F: FnMut(&mut Replayer<'_, C>),
{
    let eligible = trace.faultable();
    for seed in seeds {
        let schedule = FaultSchedule::generate(seed, &eligible, max_faults);
        if schedule.is_empty() {
            continue;
        }
        let violations = evaluate(trace, cas, schedule.clone(), invariants, &mut drive);
        if !violations.is_empty() {
            return Some(Finding {
                seed,
                schedule,
                violations,
            });
        }
    }
    None
}

/// Reduce a finding's schedule to the faults that actually matter.
///
/// The predicate is "still breaks *the same* invariants", not merely "still
/// breaks something" — otherwise the shrinker is free to wander off and
/// minimise a different bug, which is a genuinely confusing way to lose an
/// afternoon.
pub fn minimise<C, F>(
    trace: &Trace,
    cas: &mut C,
    finding: &Finding,
    invariants: &[Box<dyn Invariant>],
    mut drive: F,
) -> Shrunk
where
    C: Cas,
    F: FnMut(&mut Replayer<'_, C>),
{
    let target: Vec<String> = names_of(&finding.violations);

    shrink(&finding.schedule, |candidate| {
        let violations = evaluate(trace, cas, candidate.clone(), invariants, &mut drive);
        names_of(&violations) == target
    })
}

fn names_of(violations: &[Violation]) -> Vec<String> {
    let mut names: Vec<String> = violations.iter().map(|v| v.invariant.clone()).collect();
    names.sort();
    names.dedup();
    names
}
