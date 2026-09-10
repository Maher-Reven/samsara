//! The flagship: finding, reproducing and minimising a duplicate side effect.
//!
//! An agent deletes a file. A tool call times out, so it retries. The first
//! call had actually succeeded — the response was lost, not the request — so
//! the delete happens twice.
//!
//! In production this appears once in a few hundred runs and is never
//! reproducible. Here it is found by search, reported as an integer, and
//! reduced to a single fault.

use samsara_core::prelude::*;
use samsara_core::testkit::{run_agent, FakeBackend, Style, WorldAccess};

fn invariants() -> Vec<Box<dyn Invariant>> {
    vec![
        Box::new(NoDuplicateEffects::only(["delete_file"]).exempting_idempotent("idempotency_key")),
        Box::new(TerminatesWithin(24)),
    ]
}

/// Record one clean, uneventful run — the kind that passes review.
fn record_happy_path(style: Style) -> (Trace, MemCas) {
    let mut cas = MemCas::new();
    let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
    let report = run_agent(&mut recorder, style);
    assert!(
        report.succeeded && report.attempts == 1,
        "the happy path is happy"
    );
    (recorder.finish("delete-stale-report"), cas)
}

#[test]
fn the_happy_path_is_clean() {
    let (trace, cas) = record_happy_path(Style::Naive);
    assert!(
        check_all(&trace, &cas, &invariants()).is_empty(),
        "nothing is wrong with the recording itself — that is the problem"
    );
}

#[test]
fn search_finds_the_bug_and_shrinking_explains_it() {
    let (trace, mut cas) = record_happy_path(Style::Naive);

    let finding = search(&trace, &mut cas, 0..500u64, 4, &invariants(), |replayer| {
        run_agent(replayer, Style::Naive);
    })
    .expect("some seed must expose the duplicate delete");

    // The bug report is one integer.
    println!("{}", finding.report());
    assert!(finding
        .violations
        .iter()
        .any(|v| v.invariant == "no_duplicate_effects"));

    // Reproducing it is deterministic.
    let again = evaluate(
        &trace,
        &mut cas,
        finding.schedule.clone(),
        &invariants(),
        |r| {
            run_agent(r, Style::Naive);
        },
    );
    assert_eq!(again, finding.violations, "a seed must reproduce exactly");

    // And it minimises to a single fault: one timeout, on the delete.
    let shrunk = minimise(&trace, &mut cas, &finding, &invariants(), |r| {
        run_agent(r, Style::Naive);
    });
    println!("{}", shrunk.report());

    // The whole bug is one fault, on the delete call.
    //
    // Note what it is *not*: the search reports whichever masking fault it
    // reaches first, and that is usually an error code rather than a timeout.
    // That is the more general statement of the bug and worth stating
    // plainly — the danger is not timeouts specifically, it is **any failure
    // that hides a call which already succeeded**. A 500 from a gateway
    // after the work was done is exactly as destructive as a lost response.
    let delete_seq = trace
        .events
        .iter()
        .find(|e| e.name == "delete_file")
        .unwrap()
        .seq;

    assert_eq!(
        shrunk.schedule.len(),
        1,
        "the whole bug is one fault, got: {}",
        shrunk.schedule.describe()
    );
    assert_eq!(
        shrunk.schedule.points[0].seq, delete_seq,
        "and it is on the delete call"
    );
}

/// The shrinker earns its place only when the input is messy, and a lucky
/// search that happens to find a one-fault schedule does not demonstrate
/// that. So: bury the real culprit in noise and check ddmin digs it out.
#[test]
fn shrinking_digs_the_culprit_out_of_a_pile_of_noise() {
    let (trace, mut cas) = record_happy_path(Style::Naive);

    let delete_seq = trace
        .events
        .iter()
        .find(|e| e.name == "delete_file")
        .unwrap()
        .seq;

    // The one fault that matters, plus every irrelevant one we can aim at
    // the other effects.
    let mut points = vec![FaultPoint {
        seq: delete_seq,
        fault: Fault::Timeout,
    }];
    for (i, seq) in trace
        .faultable()
        .into_iter()
        .filter(|s| *s != delete_seq)
        .enumerate()
    {
        points.push(FaultPoint {
            seq,
            fault: match i % 3 {
                0 => Fault::Delay { ms: 900 },
                1 => Fault::Truncate { keep: 8 },
                _ => Fault::Malformed,
            },
        });
    }
    points.sort_by_key(|p| p.seq);
    let noisy = FaultSchedule::of(points);
    assert!(noisy.len() > 1, "the test needs actual noise to remove");

    let finding = Finding {
        seed: 0,
        schedule: noisy.clone(),
        violations: evaluate(&trace, &mut cas, noisy.clone(), &invariants(), |r| {
            run_agent(r, Style::Naive);
        }),
    };
    assert!(
        !finding.violations.is_empty(),
        "the noisy schedule must reproduce"
    );

    let shrunk = minimise(&trace, &mut cas, &finding, &invariants(), |r| {
        run_agent(r, Style::Naive);
    });
    println!("{}", shrunk.report());

    assert_eq!(
        shrunk.schedule.len(),
        1,
        "everything but the delete timeout is noise, got: {}",
        shrunk.schedule.describe()
    );
    assert_eq!(shrunk.schedule.points[0].seq, delete_seq);
    assert!(
        shrunk.evaluations < noisy.len() * 4,
        "ddmin should be cheap"
    );
}

#[test]
fn the_minimal_schedule_is_the_delete_call_timing_out() {
    let (trace, mut cas) = record_happy_path(Style::Naive);

    // Aim the fault by hand at the tool call and confirm that alone does it.
    let delete_seq = trace
        .events
        .iter()
        .find(|e| e.name == "delete_file")
        .expect("the recording contains a delete")
        .seq;

    let schedule = FaultSchedule::of(vec![FaultPoint {
        seq: delete_seq,
        fault: Fault::Timeout,
    }]);

    let violations = evaluate(&trace, &mut cas, schedule, &invariants(), |r| {
        run_agent(r, Style::Naive);
    });

    assert_eq!(violations.len(), 1);
    assert!(
        violations[0].detail.contains("happened twice"),
        "the shadow lets us assert this with certainty, not hedge: {}",
        violations[0].detail
    );
}

#[test]
fn the_idempotent_agent_survives_every_schedule_that_breaks_the_naive_one() {
    let (naive_trace, mut naive_cas) = record_happy_path(Style::Naive);

    let finding = search(
        &naive_trace,
        &mut naive_cas,
        0..500u64,
        4,
        &invariants(),
        |r| {
            run_agent(r, Style::Naive);
        },
    )
    .expect("the naive agent breaks");

    // Same fault, same position, against the fixed agent.
    let (fixed_trace, mut fixed_cas) = record_happy_path(Style::Idempotent);
    let violations = evaluate(
        &fixed_trace,
        &mut fixed_cas,
        finding.schedule.clone(),
        &invariants(),
        |r| {
            run_agent(r, Style::Idempotent);
        },
    );

    assert!(
        violations.is_empty(),
        "an idempotency key stable across attempts is the fix: {:?}",
        violations.iter().map(|v| v.report()).collect::<Vec<_>>()
    );
}

/// The honesty check.
///
/// Everything above happens inside replay, where no file is really deleted.
/// It would be embarrassing to ship a tool that reports a bug replay invented.
/// So: run the *same* agent against a live backend whose response is lost in
/// flight, and confirm the world really is mutated twice.
#[test]
fn the_bug_samsara_reports_is_a_real_bug() {
    // Naive agent, real backend, one lost response.
    let mut cas = MemCas::new();
    let backend = FakeBackend::new(1).lossy("delete_file", 1);
    let mut recorder = Recorder::new(backend, &mut cas, Canonicalizer::default());
    let report = run_agent(&mut recorder, Style::Naive);

    let world = recorder.world().clone();
    assert!(report.succeeded, "the agent thinks all is well");
    assert_eq!(
        world.delete_count("/var/reports/stale.csv"),
        2,
        "but the file was really deleted twice"
    );

    // The same scenario with the fixed agent mutates the world once.
    let mut cas = MemCas::new();
    let backend = FakeBackend::new(1).lossy("delete_file", 1);
    let mut recorder = Recorder::new(backend, &mut cas, Canonicalizer::default());
    let report = run_agent(&mut recorder, Style::Idempotent);

    assert!(report.succeeded);
    assert_eq!(
        recorder.world().delete_count("/var/reports/stale.csv"),
        1,
        "the idempotency key held"
    );
}
