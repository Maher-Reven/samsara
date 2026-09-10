//! The metamorphic property: **replay(record(run)) == run**.
//!
//! This is the load-bearing claim of the entire project. Everything else —
//! counterfactuals, shrinking, invariants — is worthless if replay is not
//! faithful, because every conclusion would be drawn about a run that never
//! happened.
//!
//! So it is tested as a property, over hundreds of generated configurations,
//! against real agent code, with no network and no API key.

use proptest::prelude::*;

use samsara_core::prelude::*;
use samsara_core::testkit::{run_agent, FakeBackend, Style};

/// Record a run of the agent and return everything needed to replay it.
fn record(seed: u64, style: Style, flake: usize) -> (Trace, MemCas, samsara_core::testkit::Report) {
    let mut cas = MemCas::new();
    let backend = FakeBackend::new(seed).flaky("delete_file", flake);
    let mut recorder = Recorder::new(backend, &mut cas, Canonicalizer::default());
    let report = run_agent(&mut recorder, style);
    let trace = recorder.finish("property-test");
    (trace, cas, report)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// A strict replay of a recording must be perfectly faithful: no
    /// divergences, no holes, and an identical sequence of effects.
    #[test]
    fn replay_of_a_recording_is_the_recording(
        seed in any::<u64>(),
        idempotent in any::<bool>(),
        flake in 0usize..3,
    ) {
        let style = if idempotent { Style::Idempotent } else { Style::Naive };
        let (trace, mut cas, original_report) = record(seed, style, flake);

        let mut replayer = Replayer::new(&trace, &mut cas, Mode::Strict).unwrap();
        let replay_report = run_agent(&mut replayer, style);
        let result = replayer.finish("replay");

        prop_assert!(
            result.divergences.is_empty(),
            "replay diverged: {}",
            result.divergences[0].report()
        );
        prop_assert!(result.holes.is_empty(), "replay hit {} holes", result.holes.len());

        // The agent must have behaved identically...
        prop_assert_eq!(original_report, replay_report);
        // ...and the branch must be effect-for-effect the original run.
        prop_assert_eq!(trace.id(), result.branch.id());
    }

    /// Replay must be free: it performs no work against the backend at all.
    /// If this ever failed, replay would be a slow, billable simulation
    /// rather than a recording.
    #[test]
    fn replay_touches_no_backend(seed in any::<u64>()) {
        let (trace, mut cas, _) = record(seed, Style::Naive, 0);

        let mut replayer = Replayer::new(&trace, &mut cas, Mode::Strict).unwrap();
        let _ = run_agent(&mut replayer, Style::Naive);
        let result = replayer.finish("replay");

        // A hole is the only way replay can be forced to ask for something
        // the recording cannot supply. Zero holes means zero live calls.
        prop_assert_eq!(result.holes.len(), 0);
    }

    /// Replay is idempotent: replaying twice gives the same branch both
    /// times. Trivially true only if nothing hidden leaks in — a real clock,
    /// a real RNG, a HashMap iteration order.
    #[test]
    fn replay_is_repeatable(seed in any::<u64>()) {
        let (trace, mut cas, _) = record(seed, Style::Naive, 1);

        let mut a = Replayer::new(&trace, &mut cas, Mode::Strict).unwrap();
        let _ = run_agent(&mut a, Style::Naive);
        let first = a.finish("a").branch.id();

        let mut b = Replayer::new(&trace, &mut cas, Mode::Strict).unwrap();
        let _ = run_agent(&mut b, Style::Naive);
        let second = b.finish("b").branch.id();

        prop_assert_eq!(first, second);
    }
}

/// A divergence must be *detected*, not silently tolerated. We provoke one by
/// replaying a recording of one agent against a different agent.
#[test]
fn a_different_agent_diverges_and_is_caught() {
    let (trace, mut cas, _) = record(7, Style::Naive, 0);

    let mut replayer = Replayer::new(&trace, &mut cas, Mode::Strict).unwrap();
    // The idempotent agent sends an extra argument, so its second effect has
    // a different identity than the recording's.
    let _ = run_agent(&mut replayer, Style::Idempotent);
    let result = replayer.finish("mismatched");

    let divergence = result
        .first_divergence()
        .expect("swapping the agent under test must be detected");

    assert_eq!(
        divergence.at_seq, 1,
        "the plan call matches; the tool call does not"
    );
    match &divergence.kind {
        DivergenceKind::Identity {
            first_difference, ..
        } => {
            let where_ = first_difference.as_deref().unwrap_or("");
            assert!(
                where_.contains("idempotency_key"),
                "the report must name the offending field, got {where_:?}"
            );
        }
        other => panic!("expected an identity divergence, got {other:?}"),
    }
}

/// An agent that stops early must be caught too — a replay that quietly
/// ignores unconsumed recording would hide real regressions.
#[test]
fn an_agent_that_stops_short_is_caught() {
    let (trace, mut cas, _) = record(11, Style::Naive, 0);

    let mut replayer = Replayer::new(&trace, &mut cas, Mode::Strict).unwrap();
    // Perform only the first effect, then stop.
    let _ = replayer.perform(EffectRequest::model(
        "claude-sonnet-4",
        serde_json::json!({"step": "plan", "task": "remove the stale report"}),
    ));
    let result = replayer.finish("truncated");

    assert!(matches!(
        result.first_divergence().map(|d| &d.kind),
        Some(DivergenceKind::StoppedShort { .. })
    ));
}
