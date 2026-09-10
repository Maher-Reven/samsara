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
use crate::fault::{Fault, FaultPoint, FaultSchedule};
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

// ---------------------------------------------------------------------------
// Order dependence
// ---------------------------------------------------------------------------

/// A run whose behaviour changed when concurrent calls completed in a
/// different order.
#[derive(Clone, Debug)]
pub struct OrderDependence {
    /// The batch that was reordered.
    pub batch: u64,
    /// Position in the run where that batch begins.
    pub at_seq: u64,
    /// The seed whose permutation exposed it, so the finding reproduces.
    pub seed: u64,
    /// Position of the first effect that differed from the baseline.
    pub diverged_at: usize,
    /// What the baseline did there, and what the reordered run did instead.
    pub baseline: String,
    pub reordered: String,
}

impl OrderDependence {
    pub fn report(&self) -> String {
        format!(
            "concurrent batch #{} (at effect #{}) is order-dependent \u{2014} reordering it \
             with seed {} changes effect #{}\n  in request order: {}\n  reordered:        {}",
            self.batch, self.at_seq, self.seed, self.diverged_at, self.baseline, self.reordered
        )
    }
}

/// Reduce a run to what it *did*, with the internal order of each concurrent
/// batch discarded.
///
/// This is the crux of the check. Reordering a batch trivially changes the
/// order of that batch's own effects — that is what we asked for, and
/// flagging it would report a difference on every well-behaved agent. What
/// matters is whether anything *downstream* changed. So each batch collapses
/// to a sorted multiset, and everything outside a batch keeps its position.
fn behaviour(trace: &Trace) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut current: Option<u64> = None;

    let flush = |group: &mut Vec<String>, out: &mut Vec<String>| {
        group.sort();
        out.append(group);
    };

    for event in &trace.events {
        let label = format!(
            "{}:{}:{}",
            event.kind.as_str(),
            event.name,
            event.identity.short()
        );
        match (current, event.batch) {
            (Some(a), Some(b)) if a == b => pending.push(label),
            (_, Some(b)) => {
                flush(&mut pending, &mut out);
                current = Some(b);
                pending.push(label);
            }
            (_, None) => {
                flush(&mut pending, &mut out);
                current = None;
                out.push(label);
            }
        }
    }
    flush(&mut pending, &mut out);
    out
}

/// Search for a concurrent batch whose completion order the agent depends on.
///
/// Runs the agent once with calls completing in request order, then again
/// with each batch permuted, and compares what the agent *did* afterwards.
/// A correct agent produces the same downstream effects either way; one that
/// folds results into shared state as they land does not.
///
/// This cannot be an [`Invariant`](crate::invariant::Invariant), because
/// order-dependence is not a property of one run — it is a relation between
/// two. No single trace is wrong; the pair disagrees.
pub fn search_order_dependence<C, F>(
    trace: &Trace,
    cas: &mut C,
    seeds: impl IntoIterator<Item = u64> + Clone,
    mut drive: F,
) -> Option<OrderDependence>
where
    C: Cas,
    F: FnMut(&mut Replayer<'_, C>),
{
    let run = |cas: &mut C, schedule: FaultSchedule, drive: &mut F| -> Trace {
        let mut replayer = Replayer::new(trace, cas, Mode::Counterfactual { schedule })
            .expect("replayer construction reads only the store");
        drive(&mut replayer);
        replayer.finish("order-probe").branch
    };

    let baseline = run(cas, FaultSchedule::empty(), &mut drive);
    let baseline_behaviour = behaviour(&baseline);

    // Every concurrent batch in the baseline, with where it starts.
    let mut batches: Vec<(u64, u64)> = Vec::new();
    for event in &baseline.events {
        if let Some(batch) = event.batch {
            if !batches.iter().any(|(b, _)| *b == batch) {
                batches.push((batch, event.seq));
            }
        }
    }

    for (batch, at_seq) in batches {
        for seed in seeds.clone() {
            let schedule = FaultSchedule::of(vec![FaultPoint {
                seq: at_seq,
                fault: Fault::Reorder { seed },
            }]);
            let reordered = behaviour(&run(cas, schedule, &mut drive));

            if let Some(diverged_at) = first_difference_at(&baseline_behaviour, &reordered) {
                return Some(OrderDependence {
                    batch,
                    at_seq,
                    seed,
                    diverged_at,
                    baseline: baseline_behaviour
                        .get(diverged_at)
                        .cloned()
                        .unwrap_or_else(|| "(nothing)".into()),
                    reordered: reordered
                        .get(diverged_at)
                        .cloned()
                        .unwrap_or_else(|| "(nothing)".into()),
                });
            }
        }
    }
    None
}

fn first_difference_at(a: &[String], b: &[String]) -> Option<usize> {
    (0..a.len().max(b.len())).find(|&i| a.get(i) != b.get(i))
}
