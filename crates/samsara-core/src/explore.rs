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
use serde::{Deserialize, Serialize};

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
/// This cannot be an [`Invariant`] because
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

// ---------------------------------------------------------------------------
// Exhaustive coverage
// ---------------------------------------------------------------------------

/// A schedule that broke something, found by enumeration rather than search.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Case {
    pub schedule: FaultSchedule,
    pub description: String,
    pub violations: Vec<Violation>,
}

/// What a sweep actually checked, and what it found.
///
/// The counts matter as much as the findings. "No single fault breaks this
/// agent" is only worth saying if you can also say how many single faults
/// there were, and that every one of them ran.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Coverage {
    /// Effect positions a fault can attach to.
    pub positions: usize,
    /// Distinct fault kinds tried at each position.
    pub kinds: usize,
    /// Single-fault schedules executed.
    pub singles_checked: usize,
    /// Whether every single-fault schedule ran. False only if the budget ran
    /// out, which would make the headline claim unavailable.
    pub singles_exhaustive: bool,
    /// Two-fault schedules executed.
    pub pairs_checked: usize,
    /// Whether every two-fault schedule ran.
    pub pairs_exhaustive: bool,
    /// Total replays performed.
    pub replays: usize,
    /// Everything that broke.
    pub failures: Vec<Case>,
}

impl Coverage {
    /// The strongest true statement about this run.
    ///
    /// Deliberately refuses to overstate: if the budget cut the sweep short,
    /// it says so rather than implying completeness it did not achieve.
    pub fn claim(&self) -> String {
        let mut parts = Vec::new();

        let singles = if self.singles_exhaustive {
            match self
                .failures
                .iter()
                .filter(|c| c.schedule.len() == 1)
                .count()
            {
                0 => format!(
                    "no single fault breaks this agent \u{2014} all {} were checked",
                    self.singles_checked
                ),
                n => format!("{n} of {} single faults break it", self.singles_checked),
            }
        } else {
            format!(
                "{} of the single faults checked (budget reached, not exhaustive)",
                self.singles_checked
            )
        };
        parts.push(singles);

        if self.pairs_checked > 0 {
            let broken = self
                .failures
                .iter()
                .filter(|c| c.schedule.len() == 2)
                .count();
            let scope = if self.pairs_exhaustive {
                "all"
            } else {
                "a sample of"
            };
            parts.push(match broken {
                0 => format!("no pair does either, across {scope} {}", self.pairs_checked),
                n => format!("{n} pairs do, across {scope} {}", self.pairs_checked),
            });
        }
        parts.join("; ")
    }

    /// Whether anything broke.
    pub fn is_clean(&self) -> bool {
        self.failures.is_empty()
    }
}

/// Check **every** single-fault schedule, then as many pairs as the budget
/// allows.
///
/// This is the difference between testing and verification, and it is
/// available only because replay is free: a run costs no tokens and no
/// network, so enumerating hundreds of them is a second of CPU rather than a
/// bill. Random seed search is what you do when each attempt is expensive.
/// Nothing here is expensive.
/// The complete list of schedules a sweep will run.
///
/// Separated from execution so that a driver which cannot hand out a
/// `Replayer` -- an agent in another process, say -- enumerates exactly the
/// same space as the in-process one. Two enumerations that could drift would
/// mean two different definitions of "exhaustive".
#[derive(Clone, Debug)]
pub struct Plan {
    pub positions: usize,
    pub kinds: usize,
    pub singles: Vec<FaultSchedule>,
    pub pairs: Vec<FaultSchedule>,
}

/// Enumerate every single fault, and every pair if asked.
pub fn plan(trace: &Trace, include_pairs: bool) -> Plan {
    let positions = trace.faultable();
    let kinds = Fault::canonical_set();

    let mut singles = Vec::new();
    for &seq in &positions {
        for fault in &kinds {
            singles.push(FaultSchedule::of(vec![FaultPoint {
                seq,
                fault: fault.clone(),
            }]));
        }
    }

    let mut pairs = Vec::new();
    if include_pairs {
        for (i, &a) in positions.iter().enumerate() {
            for &b in &positions[i + 1..] {
                for fa in &kinds {
                    for fb in &kinds {
                        pairs.push(FaultSchedule::of(vec![
                            FaultPoint {
                                seq: a,
                                fault: fa.clone(),
                            },
                            FaultPoint {
                                seq: b,
                                fault: fb.clone(),
                            },
                        ]));
                    }
                }
            }
        }
    }

    Plan {
        positions: positions.len(),
        kinds: kinds.len(),
        singles,
        pairs,
    }
}

impl Coverage {
    /// Start an empty tally for `plan`.
    pub fn starting(plan: &Plan) -> Coverage {
        Coverage {
            positions: plan.positions,
            kinds: plan.kinds,
            singles_checked: 0,
            singles_exhaustive: false,
            pairs_checked: 0,
            pairs_exhaustive: false,
            replays: 0,
            failures: Vec::new(),
        }
    }

    /// Record one executed schedule.
    pub fn record(&mut self, schedule: FaultSchedule, violations: Vec<Violation>) {
        self.replays += 1;
        match schedule.len() {
            0 | 1 => self.singles_checked += 1,
            _ => self.pairs_checked += 1,
        }
        if !violations.is_empty() {
            self.failures.push(Case {
                description: schedule.describe(),
                schedule,
                violations,
            });
        }
    }
}

pub fn sweep<C, F>(
    trace: &Trace,
    cas: &mut C,
    invariants: &[Box<dyn Invariant>],
    max_replays: usize,
    include_pairs: bool,
    mut drive: F,
) -> Coverage
where
    C: Cas,
    F: FnMut(&mut Replayer<'_, C>),
{
    let plan = plan(trace, include_pairs);
    let mut coverage = Coverage::starting(&plan);
    coverage.singles_exhaustive = true;

    let positions = trace.faultable();
    let kinds = Fault::canonical_set();

    let mut run = |schedule: FaultSchedule, cov: &mut Coverage, drive: &mut F| {
        cov.replays += 1;
        let violations = evaluate(trace, cas, schedule.clone(), invariants, drive);
        if !violations.is_empty() {
            cov.failures.push(Case {
                description: schedule.describe(),
                schedule,
                violations,
            });
        }
    };

    // --- every single fault ---
    'singles: for &seq in &positions {
        for fault in &kinds {
            if coverage.replays >= max_replays {
                coverage.singles_exhaustive = false;
                break 'singles;
            }
            let schedule = FaultSchedule::of(vec![FaultPoint {
                seq,
                fault: fault.clone(),
            }]);
            run(schedule, &mut coverage, &mut drive);
            coverage.singles_checked += 1;
        }
    }

    if !include_pairs || !coverage.singles_exhaustive {
        return coverage;
    }

    // --- every pair of positions, every combination of kinds ---
    let mut exhaustive = true;
    'pairs: for (i, &a) in positions.iter().enumerate() {
        for &b in &positions[i + 1..] {
            for fa in &kinds {
                for fb in &kinds {
                    if coverage.replays >= max_replays {
                        exhaustive = false;
                        break 'pairs;
                    }
                    let schedule = FaultSchedule::of(vec![
                        FaultPoint {
                            seq: a,
                            fault: fa.clone(),
                        },
                        FaultPoint {
                            seq: b,
                            fault: fb.clone(),
                        },
                    ]);
                    run(schedule, &mut coverage, &mut drive);
                    coverage.pairs_checked += 1;
                }
            }
        }
    }
    coverage.pairs_exhaustive = exhaustive;
    coverage
}

// ---------------------------------------------------------------------------
// Exhaustive interleaving
// ---------------------------------------------------------------------------

/// What an interleaving check covered.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Interleavings {
    /// Batch id.
    pub batch: u64,
    /// Number of concurrent calls in it.
    pub width: usize,
    /// Total orderings that exist: `width!`.
    pub total: usize,
    /// Orderings actually executed.
    pub checked: usize,
    /// True when every ordering ran, so "order-independent" is a statement
    /// about all of them rather than about a sample.
    pub exhaustive: bool,
    /// Pairs of positions that commute: swapping them adjacently left the
    /// agent's downstream behaviour unchanged.
    pub commuting_pairs: usize,
    /// Total adjacent pairs examined.
    pub adjacent_pairs: usize,
    /// Orderings under which the agent behaved differently from the
    /// recorded one.
    pub divergent: Vec<Vec<usize>>,
    /// Replays this check cost. Reported separately from the fault sweep so
    /// a total is a total rather than a subset presented as one.
    pub replays: usize,
}

impl Interleavings {
    pub fn is_order_independent(&self) -> bool {
        self.divergent.is_empty()
    }

    pub fn claim(&self) -> String {
        let scope = if self.exhaustive {
            format!("all {} orderings", self.total)
        } else {
            format!("{} of {} orderings", self.checked, self.total)
        };
        if self.divergent.is_empty() {
            format!("batch #{} is order-independent across {scope}", self.batch)
        } else {
            format!(
                "batch #{} behaves differently under {} of {scope}",
                self.batch,
                self.divergent.len()
            )
        }
    }
}

/// Largest batch width worth enumerating exhaustively.
///
/// 7! is 5040 replays, which is about a second. 8! is eight times that and
/// the returns stop justifying it; beyond this the check reports a sample
/// and says so rather than quietly pretending.
const EXHAUSTIVE_WIDTH: usize = 7;

/// Check a concurrent batch against **every** ordering.
///
/// The claim this supports is categorically stronger than random
/// permutation: not "it survived two hundred shuffles" but "there is no
/// ordering under which it behaves differently", which for a batch of five
/// is a statement about all one hundred and twenty.
///
/// It is affordable for the same reason the fault sweep is: a replay costs
/// nothing. Exhaustive checking is normally out of reach because each trial
/// is expensive. Here no trial is.
pub fn interleavings<C, F>(trace: &Trace, cas: &mut C, mut drive: F) -> Vec<Interleavings>
where
    C: Cas,
    F: FnMut(&mut Replayer<'_, C>),
{
    let run = |cas: &mut C, schedule: FaultSchedule, drive: &mut F| -> Vec<String> {
        let mut replayer = Replayer::new(trace, cas, Mode::Counterfactual { schedule })
            .expect("replayer construction reads only the store");
        drive(&mut replayer);
        behaviour(&replayer.finish("interleaving").branch)
    };

    let baseline = run(cas, FaultSchedule::empty(), &mut drive);

    // Every batch in the recording, with where it starts and how wide it is.
    let mut batches: Vec<(u64, u64, usize)> = Vec::new();
    for event in &trace.events {
        if let Some(batch) = event.batch {
            match batches.iter_mut().find(|(b, _, _)| *b == batch) {
                Some((_, _, width)) => *width += 1,
                None => batches.push((batch, event.seq, 1)),
            }
        }
    }

    let mut reports = Vec::new();
    for (batch, at_seq, width) in batches {
        let total = (1..=width).product::<usize>();
        let exhaustive = width <= EXHAUSTIVE_WIDTH;

        let mut checked = 0usize;
        let mut divergent = Vec::new();

        let orders: Vec<Vec<usize>> = if exhaustive {
            permutations(width)
        } else {
            // Beyond the bound, fall back to seeded sampling and say so.
            (0..512u64)
                .filter_map(|seed| Fault::Reorder { seed }.permutation(width))
                .collect()
        };

        for order in &orders {
            // The identity ordering is the recording; running it proves
            // nothing and would count toward coverage dishonestly.
            if order.iter().enumerate().all(|(i, &j)| i == j) {
                continue;
            }
            let schedule = FaultSchedule::of(vec![FaultPoint {
                seq: at_seq,
                fault: Fault::Exact {
                    order: order.clone(),
                },
            }]);
            checked += 1;
            if run(cas, schedule, &mut drive) != baseline {
                divergent.push(order.clone());
            }
        }

        // Which adjacent pairs commute? Cheap to compute from what we ran,
        // and the useful diagnostic: it says *which* two calls are entangled
        // rather than only that something is.
        let mut commuting = 0usize;
        let mut adjacent = 0usize;
        for i in 0..width.saturating_sub(1) {
            adjacent += 1;
            let mut order: Vec<usize> = (0..width).collect();
            order.swap(i, i + 1);
            let schedule = FaultSchedule::of(vec![FaultPoint {
                seq: at_seq,
                fault: Fault::Exact { order },
            }]);
            if run(cas, schedule, &mut drive) == baseline {
                commuting += 1;
            }
        }

        reports.push(Interleavings {
            batch,
            width,
            total,
            checked: checked + 1, // the recorded ordering counts as covered
            exhaustive,
            commuting_pairs: commuting,
            adjacent_pairs: adjacent,
            divergent,
            replays: checked + adjacent,
        });
    }
    reports
}

/// All permutations of `0..n`, in a deterministic order.
fn permutations(n: usize) -> Vec<Vec<usize>> {
    let mut out = Vec::new();
    let mut current: Vec<usize> = (0..n).collect();
    heap(&mut current, n, &mut out);
    out.sort();
    out
}

/// Heap's algorithm.
fn heap(items: &mut Vec<usize>, k: usize, out: &mut Vec<Vec<usize>>) {
    if k <= 1 {
        out.push(items.clone());
        return;
    }
    for i in 0..k {
        heap(items, k - 1, out);
        if k % 2 == 0 {
            items.swap(i, k - 1);
        } else {
            items.swap(0, k - 1);
        }
    }
}
