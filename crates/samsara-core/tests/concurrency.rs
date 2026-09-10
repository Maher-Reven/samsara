//! Order-dependent bugs in concurrent tool calls.
//!
//! An agent asks the model to plan a report, renders three sections
//! concurrently, and saves the result. Every call succeeds. Nothing times
//! out, nothing retries, no budget is exceeded, and no invariant about
//! duplicate side effects fires.
//!
//! The document is simply assembled in the wrong order, and only sometimes.
//! In production that reads as an intermittent formatting glitch nobody can
//! reproduce — which is exactly the failure mode this project exists for.

use samsara_core::prelude::*;
use samsara_core::testkit::{run_assembler, Assembly, FakeBackend};

fn record(assembly: Assembly) -> (Trace, MemCas, Vec<String>) {
    let mut cas = MemCas::new();
    let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
    let document = run_assembler(&mut recorder, assembly);
    (recorder.finish("assemble-report"), cas, document)
}

#[test]
fn the_recording_groups_concurrent_calls_into_a_batch() {
    let (trace, _, document) = record(Assembly::AsTheyArrive);

    let batched: Vec<&Event> = trace.events.iter().filter(|e| e.batch.is_some()).collect();
    assert_eq!(
        batched.len(),
        3,
        "three sections were rendered concurrently"
    );
    assert!(
        batched.iter().all(|e| e.batch == batched[0].batch),
        "and they share one batch id"
    );
    assert!(
        trace
            .events
            .iter()
            .any(|e| e.name == "save_document" && e.batch.is_none()),
        "the save that follows is not part of the batch"
    );
    assert_eq!(document, vec!["<intro>", "<summary>", "<appendix>"]);
}

#[test]
fn strict_replay_of_a_concurrent_run_is_still_faithful() {
    let (trace, mut cas, document) = record(Assembly::AsTheyArrive);

    let mut replayer = Replayer::new(&trace, &mut cas, Mode::Strict).unwrap();
    let replayed = run_assembler(&mut replayer, Assembly::AsTheyArrive);
    let result = replayer.finish("replay");

    assert!(
        result.divergences.is_empty(),
        "unfaulted concurrent replay must stay in lockstep: {}",
        result.divergences[0].report()
    );
    assert_eq!(document, replayed);
    assert_eq!(trace.id(), result.branch.id());
}

#[test]
fn reordering_changes_what_the_naive_agent_saves() {
    let (trace, mut cas, _) = record(Assembly::AsTheyArrive);

    let batch_start = trace
        .events
        .iter()
        .find(|e| e.batch.is_some())
        .expect("there is a batch")
        .seq;

    let schedule = FaultSchedule::of(vec![FaultPoint {
        seq: batch_start,
        fault: Fault::Reorder { seed: 7 },
    }]);

    let mut replayer = Replayer::new(&trace, &mut cas, Mode::Counterfactual { schedule }).unwrap();
    let document = run_assembler(&mut replayer, Assembly::AsTheyArrive);
    let result = replayer.finish("reordered");

    assert_ne!(
        document,
        vec!["<intro>", "<summary>", "<appendix>"],
        "the document must come out in a different order"
    );
    assert_eq!(
        document.len(),
        3,
        "every section still rendered — nothing failed, it is purely an ordering bug"
    );

    // And the damage is visible downstream: the save carries different bytes.
    let saved = result
        .branch
        .events
        .iter()
        .find(|e| e.name == "save_document")
        .expect("the agent still saved");
    let original = trace
        .events
        .iter()
        .find(|e| e.name == "save_document")
        .unwrap();
    assert_ne!(
        saved.identity, original.identity,
        "a different document was written"
    );
}

#[test]
fn the_search_finds_the_order_dependence() {
    let (trace, mut cas, _) = record(Assembly::AsTheyArrive);

    let finding = search_order_dependence(&trace, &mut cas, 0..40u64, |replayer| {
        run_assembler(replayer, Assembly::AsTheyArrive);
    })
    .expect("the naive agent depends on completion order");

    println!("{}", finding.report());
    assert!(finding.baseline.contains("save_document"));
    assert!(finding.reordered.contains("save_document"));

    // The finding must reproduce from its seed.
    let schedule = FaultSchedule::of(vec![FaultPoint {
        seq: finding.at_seq,
        fault: Fault::Reorder { seed: finding.seed },
    }]);
    let mut replayer = Replayer::new(&trace, &mut cas, Mode::Counterfactual { schedule }).unwrap();
    let document = run_assembler(&mut replayer, Assembly::AsTheyArrive);
    let _ = replayer.finish("repro");
    assert_ne!(document, vec!["<intro>", "<summary>", "<appendix>"]);
}

#[test]
fn the_order_independent_agent_survives_every_permutation() {
    let (trace, mut cas, document) = record(Assembly::ByRequestOrder);
    assert_eq!(document, vec!["<intro>", "<summary>", "<appendix>"]);

    let finding = search_order_dependence(&trace, &mut cas, 0..40u64, |replayer| {
        run_assembler(replayer, Assembly::ByRequestOrder);
    });

    assert!(
        finding.is_none(),
        "placing results by request index is the fix: {}",
        finding.map(|f| f.report()).unwrap_or_default()
    );
}

#[test]
fn reordering_suppresses_nothing_and_fails_nothing_it_touched() {
    // A scheduling fault must not be mistaken for an outcome fault. If it
    // set a shadow, the duplicate-effect check would start hedging about
    // calls that plainly succeeded.
    let (trace, mut cas, _) = record(Assembly::AsTheyArrive);
    let batch_start = trace.events.iter().find(|e| e.batch.is_some()).unwrap().seq;

    let schedule = FaultSchedule::of(vec![FaultPoint {
        seq: batch_start,
        fault: Fault::Reorder { seed: 3 },
    }]);
    let mut replayer = Replayer::new(&trace, &mut cas, Mode::Counterfactual { schedule }).unwrap();
    run_assembler(&mut replayer, Assembly::AsTheyArrive);
    let result = replayer.finish("reordered");

    assert!(
        result.branch.events.iter().all(|e| e.shadow.is_none()),
        "reordering suppresses nothing, so no event may carry a shadow"
    );

    // Every reordered call itself still succeeded: they are the same calls,
    // served by identity, just in a different sequence.
    let batch_ok = result
        .branch
        .events
        .iter()
        .filter(|e| e.batch.is_some())
        .filter_map(|e| cas.get(&e.outcome).ok().flatten())
        .filter_map(|b| serde_json::from_slice::<Outcome>(&b).ok())
        .all(|o| o.is_ok());
    assert!(batch_ok, "the reordered calls all succeeded");
}

#[test]
fn order_dependence_shows_up_downstream_as_an_unanswerable_effect() {
    // Worth pinning, because it is how the bug announces itself and it looks
    // like a limitation until you see why.
    //
    // Reordering makes the naive agent assemble a different document, so its
    // `save_document` call carries different arguments — a different effect
    // identity, one the recording never saw. The oracle cannot answer it, so
    // it is reported as a hole.
    //
    // That hole *is* the finding. A run that asks a question its recording
    // has no answer to is a run that has left the behaviour we observed. To
    // see what the agent would go on to do, re-record against a live backend.
    let (trace, mut cas, _) = record(Assembly::AsTheyArrive);
    let batch_start = trace.events.iter().find(|e| e.batch.is_some()).unwrap().seq;

    let schedule = FaultSchedule::of(vec![FaultPoint {
        seq: batch_start,
        fault: Fault::Reorder { seed: 3 },
    }]);
    let mut replayer = Replayer::new(&trace, &mut cas, Mode::Counterfactual { schedule }).unwrap();
    run_assembler(&mut replayer, Assembly::AsTheyArrive);
    let result = replayer.finish("reordered");

    assert_eq!(result.holes.len(), 1, "exactly one unanswerable effect");
    assert_eq!(result.holes[0].name, "save_document");

    // The fixed agent asks nothing new, so it has no holes.
    let (fixed_trace, mut fixed_cas, _) = record(Assembly::ByRequestOrder);
    let batch_start = fixed_trace
        .events
        .iter()
        .find(|e| e.batch.is_some())
        .unwrap()
        .seq;
    let schedule = FaultSchedule::of(vec![FaultPoint {
        seq: batch_start,
        fault: Fault::Reorder { seed: 3 },
    }]);
    let mut replayer = Replayer::new(
        &fixed_trace,
        &mut fixed_cas,
        Mode::Counterfactual { schedule },
    )
    .unwrap();
    run_assembler(&mut replayer, Assembly::ByRequestOrder);
    assert!(
        replayer.finish("reordered").holes.is_empty(),
        "an order-independent agent asks the same questions in any order"
    );
}

#[test]
fn a_permutation_is_deterministic_and_never_the_identity() {
    for seed in 0..200u64 {
        for n in 2..6usize {
            let fault = Fault::Reorder { seed };
            let a = fault.permutation(n).expect("reorderable");
            let b = fault.permutation(n).expect("reorderable");

            assert_eq!(a, b, "same seed, same permutation");

            let mut sorted = a.clone();
            sorted.sort_unstable();
            assert_eq!(sorted, (0..n).collect::<Vec<_>>(), "must be a permutation");

            assert!(
                a.iter().enumerate().any(|(i, &j)| i != j),
                "a reordering that reorders nothing is a fault the shrinker would rightly drop"
            );
        }
    }
    // Nothing to permute.
    assert!(Fault::Reorder { seed: 1 }.permutation(1).is_none());
    assert!(
        Fault::Timeout.permutation(4).is_none(),
        "not a scheduling fault"
    );
}
