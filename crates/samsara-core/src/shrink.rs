//! Delta debugging over fault schedules.
//!
//! Finding a failure is the easy half. A random schedule that breaks the agent
//! usually contains a dozen faults, eleven of which are irrelevant, and a bug
//! report nobody can read is a bug nobody fixes.
//!
//! `ddmin` (Zeller & Hildebrandt, *Simplifying and Isolating Failure-Inducing
//! Input*, 2002) reduces a failing input to a 1-minimal one: every remaining
//! element is load-bearing, in the sense that removing any single one makes
//! the failure go away. That is the difference between
//!
//! ```text
//! #1 delay(812ms), #3 timeout, #4 error(503), #7 truncate(12B), ... (11 more)
//! ```
//!
//! and
//!
//! ```text
//! #3 timeout
//! ```

use crate::fault::{FaultPoint, FaultSchedule};

/// Outcome of a shrink, with the arithmetic that justifies it.
#[derive(Clone, Debug)]
pub struct Shrunk {
    /// The 1-minimal failing schedule.
    pub schedule: FaultSchedule,
    /// How many faults we started with.
    pub started_with: usize,
    /// How many times the predicate was evaluated. Each evaluation is a full
    /// replay, so this is the cost of the shrink.
    pub evaluations: usize,
}

impl Shrunk {
    pub fn removed(&self) -> usize {
        self.started_with.saturating_sub(self.schedule.len())
    }

    pub fn report(&self) -> String {
        format!(
            "shrank {} faults to {} in {} replays\n  {}",
            self.started_with,
            self.schedule.len(),
            self.evaluations,
            self.schedule.describe()
        )
    }
}

/// Reduce `schedule` to a 1-minimal subset that still satisfies `fails`.
///
/// `fails` must be deterministic — the same subset must always give the same
/// answer — which is exactly what the rest of Samsara guarantees, and is why
/// shrinking is possible here at all. Against a live API it would not be.
///
/// If `fails` does not hold for the input, the input is returned untouched:
/// there is nothing to minimise about a schedule that does not reproduce.
pub fn shrink<F>(schedule: &FaultSchedule, mut fails: F) -> Shrunk
where
    F: FnMut(&FaultSchedule) -> bool,
{
    let started_with = schedule.len();
    let mut evaluations = 0usize;

    let mut probe = |points: &[FaultPoint], evaluations: &mut usize| -> bool {
        *evaluations += 1;
        fails(&FaultSchedule::of(points.to_vec()))
    };

    if !probe(&schedule.points, &mut evaluations) {
        return Shrunk {
            schedule: schedule.clone(),
            started_with,
            evaluations,
        };
    }

    let mut current: Vec<FaultPoint> = schedule.points.clone();
    let mut granularity = 2usize;

    while current.len() >= 2 {
        let chunks = partition(&current, granularity.min(current.len()));
        let mut reduced = false;

        // Does one chunk alone reproduce? Best case: an n-fold reduction.
        for chunk in &chunks {
            if chunk.is_empty() {
                continue;
            }
            if probe(chunk, &mut evaluations) {
                current = chunk.clone();
                granularity = 2;
                reduced = true;
                break;
            }
        }

        // Otherwise, can we delete a chunk and still reproduce?
        if !reduced {
            for chunk in &chunks {
                let complement: Vec<FaultPoint> = current
                    .iter()
                    .filter(|p| !chunk.contains(p))
                    .cloned()
                    .collect();
                if complement.len() < current.len()
                    && !complement.is_empty()
                    && probe(&complement, &mut evaluations)
                {
                    current = complement;
                    granularity = (granularity - 1).max(2);
                    reduced = true;
                    break;
                }
            }
        }

        if !reduced {
            if granularity >= current.len() {
                break; // 1-minimal: no single element can be dropped.
            }
            granularity = (granularity * 2).min(current.len());
        }
    }

    Shrunk {
        schedule: FaultSchedule::of(current),
        started_with,
        evaluations,
    }
}

/// Split into `n` contiguous chunks of near-equal size.
fn partition<T: Clone>(items: &[T], n: usize) -> Vec<Vec<T>> {
    if n == 0 || items.is_empty() {
        return vec![];
    }
    let n = n.min(items.len());
    let base = items.len() / n;
    let extra = items.len() % n;

    let mut chunks = Vec::with_capacity(n);
    let mut start = 0;
    for i in 0..n {
        // The first `extra` chunks take one more, so sizes differ by at most 1.
        let size = base + usize::from(i < extra);
        chunks.push(items[start..start + size].to_vec());
        start += size;
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fault::Fault;

    fn sched(seqs: &[u64]) -> FaultSchedule {
        FaultSchedule::of(
            seqs.iter()
                .map(|&seq| FaultPoint {
                    seq,
                    fault: Fault::Timeout,
                })
                .collect(),
        )
    }

    fn seqs_of(s: &FaultSchedule) -> Vec<u64> {
        s.points.iter().map(|p| p.seq).collect()
    }

    #[test]
    fn isolates_a_single_culprit_from_a_crowd() {
        let input = sched(&(0..32).collect::<Vec<_>>());
        // Only the presence of #17 matters.
        let out = shrink(&input, |s| s.points.iter().any(|p| p.seq == 17));

        assert_eq!(seqs_of(&out.schedule), vec![17]);
        assert_eq!(out.started_with, 32);
        assert!(
            out.evaluations < 32,
            "ddmin should beat linear search, took {}",
            out.evaluations
        );
    }

    #[test]
    fn isolates_a_pair_that_only_fails_together() {
        let input = sched(&(0..24).collect::<Vec<_>>());
        let out = shrink(&input, |s| {
            let have: Vec<u64> = seqs_of(s);
            have.contains(&3) && have.contains(&19)
        });
        let mut got = seqs_of(&out.schedule);
        got.sort_unstable();
        assert_eq!(got, vec![3, 19]);
    }

    #[test]
    fn a_non_reproducing_input_is_returned_untouched() {
        let input = sched(&[1, 2, 3]);
        let out = shrink(&input, |_| false);
        assert_eq!(out.schedule, input);
        assert_eq!(
            out.evaluations, 1,
            "one probe is enough to learn it never fails"
        );
    }

    #[test]
    fn an_always_failing_predicate_shrinks_to_nothing_or_one() {
        let input = sched(&[1, 2, 3, 4]);
        let out = shrink(&input, |_| true);
        assert!(out.schedule.len() <= 1, "got {:?}", seqs_of(&out.schedule));
    }

    #[test]
    fn result_is_one_minimal() {
        // Fails if any two of {2,5,9} are present.
        let input = sched(&(0..16).collect::<Vec<_>>());
        let target = [2u64, 5, 9];
        let out = shrink(&input, |s| {
            seqs_of(s).iter().filter(|q| target.contains(q)).count() >= 2
        });

        // Removing any single remaining fault must break reproduction.
        for i in 0..out.schedule.len() {
            let mut lesser = out.schedule.points.clone();
            lesser.remove(i);
            let smaller = FaultSchedule::of(lesser);
            let still = seqs_of(&smaller)
                .iter()
                .filter(|q| target.contains(q))
                .count()
                >= 2;
            assert!(!still, "not 1-minimal: could still drop #{i}");
        }
    }

    #[test]
    fn partition_covers_everything_exactly_once() {
        for n in 1..=7 {
            let items: Vec<u64> = (0..13).collect();
            let chunks = partition(&items, n);
            let flat: Vec<u64> = chunks.iter().flatten().copied().collect();
            assert_eq!(flat, items, "n={n}");
            let sizes: Vec<usize> = chunks.iter().map(|c| c.len()).collect();
            let (lo, hi) = (sizes.iter().min().unwrap(), sizes.iter().max().unwrap());
            assert!(hi - lo <= 1, "chunks must be balanced, got {sizes:?}");
        }
    }
}
