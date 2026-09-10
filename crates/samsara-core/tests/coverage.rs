//! Exhaustive coverage: checking everything rather than sampling.
//!
//! Every tool in this space injects faults at random and reports what broke.
//! That answers "is there a bug here?" but never "is there no bug here?" —
//! and the second question is the one anyone shipping an agent actually has.
//!
//! Exhaustive checking is normally out of reach because each trial costs
//! something. Here a replay costs nothing: no tokens, no network, no clock.
//! So the whole finite fault space can simply be run.

use samsara_core::prelude::*;
use samsara_core::testkit::{run_agent, run_assembler, Assembly, FakeBackend, Style};

fn invariants() -> Vec<Box<dyn Invariant>> {
    vec![
        Box::new(NoDuplicateEffects::only(["delete_file"]).exempting_idempotent("idempotency_key")),
        Box::new(TerminatesWithin(24)),
    ]
}

fn record(style: Style) -> (Trace, MemCas) {
    let mut cas = MemCas::new();
    let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
    run_agent(&mut recorder, style);
    (recorder.finish("delete-stale-report"), cas)
}

// ---------------------------------------------------------------------------
// Fault coverage
// ---------------------------------------------------------------------------

#[test]
fn the_sweep_checks_every_single_fault() {
    let (trace, mut cas) = record(Style::Naive);

    let coverage = sweep(&trace, &mut cas, &invariants(), 10_000, false, |r| {
        run_agent(r, Style::Naive);
    });

    // The arithmetic must add up, or the completeness claim is empty.
    assert!(coverage.singles_exhaustive);
    assert_eq!(
        coverage.singles_checked,
        coverage.positions * coverage.kinds,
        "every position crossed with every fault kind"
    );
    assert_eq!(coverage.replays, coverage.singles_checked);
    assert!(coverage.positions > 0 && coverage.kinds > 0);
}

#[test]
fn the_naive_agent_is_broken_by_single_faults_and_says_which() {
    let (trace, mut cas) = record(Style::Naive);

    let coverage = sweep(&trace, &mut cas, &invariants(), 10_000, false, |r| {
        run_agent(r, Style::Naive);
    });

    assert!(!coverage.is_clean(), "the retry bug must be found");
    let claim = coverage.claim();
    println!("{claim}");
    assert!(claim.contains("single faults break it"), "{claim}");

    // Every failing case must be a fault that *masks* a call which really
    // happened. Truncation and duplication do not make the agent retry.
    for case in &coverage.failures {
        let fault = &case.schedule.points[0].fault;
        assert!(
            matches!(fault, Fault::Timeout | Fault::Error { .. }),
            "unexpected culprit: {}",
            case.description
        );
    }
}

#[test]
fn the_fixed_agent_survives_every_single_fault_and_every_pair() {
    let (trace, mut cas) = record(Style::Idempotent);

    let coverage = sweep(&trace, &mut cas, &invariants(), 100_000, true, |r| {
        run_agent(r, Style::Idempotent);
    });

    let claim = coverage.claim();
    println!("{claim} ({} replays)", coverage.replays);

    assert!(coverage.is_clean(), "the fixed agent must survive: {claim}");
    assert!(coverage.singles_exhaustive);
    assert!(
        coverage.pairs_exhaustive,
        "the pair space must be completed too"
    );
    assert!(
        claim.contains("no single fault breaks this agent"),
        "this sentence is the whole product: {claim}"
    );
}

#[test]
fn a_budget_that_runs_out_refuses_to_claim_completeness() {
    // The failure mode that would matter most: a sweep that gets cut short
    // and reports as though it had finished.
    let (trace, mut cas) = record(Style::Idempotent);

    let coverage = sweep(&trace, &mut cas, &invariants(), 3, false, |r| {
        run_agent(r, Style::Idempotent);
    });

    assert!(!coverage.singles_exhaustive);
    assert_eq!(coverage.replays, 3);
    let claim = coverage.claim();
    assert!(claim.contains("not exhaustive"), "{claim}");
    assert!(
        !claim.contains("no single fault breaks this agent"),
        "must not claim completeness it did not reach: {claim}"
    );
}

#[test]
fn pairs_are_only_attempted_once_singles_are_complete() {
    // A pair result computed on top of an incomplete single sweep would be
    // reporting depth it has not earned.
    let (trace, mut cas) = record(Style::Idempotent);
    let coverage = sweep(&trace, &mut cas, &invariants(), 3, true, |r| {
        run_agent(r, Style::Idempotent);
    });
    assert_eq!(coverage.pairs_checked, 0);
    assert!(!coverage.pairs_exhaustive);
}

// ---------------------------------------------------------------------------
// Interleaving coverage
// ---------------------------------------------------------------------------

fn record_assembly(assembly: Assembly) -> (Trace, MemCas) {
    let mut cas = MemCas::new();
    let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
    run_assembler(&mut recorder, assembly);
    (recorder.finish("assemble-report"), cas)
}

#[test]
fn every_ordering_of_a_batch_is_checked() {
    let (trace, mut cas) = record_assembly(Assembly::ByRequestOrder);

    let reports = interleavings(&trace, &mut cas, |r| {
        run_assembler(r, Assembly::ByRequestOrder);
    });

    assert_eq!(reports.len(), 1, "one concurrent batch");
    let report = &reports[0];
    assert_eq!(report.width, 3);
    assert_eq!(report.total, 6, "3! orderings");
    assert!(report.exhaustive);
    assert_eq!(report.checked, 6, "all of them, counting the recorded one");

    assert!(report.is_order_independent());
    let claim = report.claim();
    println!("{claim}");
    assert!(claim.contains("all 6 orderings"), "{claim}");
}

#[test]
fn a_dependent_batch_reports_which_orderings_break_it() {
    let (trace, mut cas) = record_assembly(Assembly::AsTheyArrive);

    let reports = interleavings(&trace, &mut cas, |r| {
        run_assembler(r, Assembly::AsTheyArrive);
    });
    let report = &reports[0];

    println!("{}", report.claim());
    assert!(!report.is_order_independent());
    assert_eq!(
        report.divergent.len(),
        5,
        "every ordering but the recorded one changes the document"
    );
    assert_eq!(
        report.commuting_pairs, 0,
        "no two of these calls commute — the agent folds them in as they land"
    );
    assert_eq!(report.adjacent_pairs, 2);
}

#[test]
fn an_independent_batch_reports_every_pair_as_commuting() {
    let (trace, mut cas) = record_assembly(Assembly::ByRequestOrder);
    let reports = interleavings(&trace, &mut cas, |r| {
        run_assembler(r, Assembly::ByRequestOrder);
    });
    assert_eq!(reports[0].commuting_pairs, reports[0].adjacent_pairs);
}

#[test]
fn a_trace_with_no_concurrency_has_nothing_to_interleave() {
    let (trace, mut cas) = record(Style::Naive);
    let reports = interleavings(&trace, &mut cas, |r| {
        run_agent(r, Style::Naive);
    });
    assert!(reports.is_empty());
}

#[test]
fn an_exact_ordering_is_used_verbatim_and_a_malformed_one_ignored() {
    let good = Fault::Exact {
        order: vec![2, 0, 1],
    };
    assert_eq!(good.permutation(3), Some(vec![2, 0, 1]));

    // Wrong width, repeated index, out of range: each would index out of
    // bounds in the scheduler if it were honoured.
    for bad in [vec![0, 1], vec![0, 0, 1], vec![0, 1, 9]] {
        assert_eq!(Fault::Exact { order: bad }.permutation(3), None);
    }
}
