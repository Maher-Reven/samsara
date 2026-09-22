//! Decisions balanced on a threshold.
//!
//! Models that return typed answers with confidence scores make a particular
//! shape of agent easy to write: `if confidence > 0.8 { delete }`. Nothing in
//! that line is wrong, and no amount of breaking the transport will find the
//! problem with it — every call succeeds, nothing times out, nothing retries.
//!
//! The problem is that a destructive action is balanced on a margin far finer
//! than the model's own precision, and nobody notices because the recording
//! came back at 0.86.

use samsara_core::prelude::*;
use samsara_core::testkit::{run_classifier, FakeBackend, Gate, CONFIDENCE_BAR};

fn record(gate: Gate) -> (Trace, MemCas, &'static str) {
    let mut cas = MemCas::new();
    let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
    let did = run_classifier(&mut recorder, gate);
    (recorder.finish("classify-and-delete"), cas, did)
}

fn bar() -> Vec<BoundarySpec> {
    vec![BoundarySpec {
        field: "confidence".into(),
        thresholds: vec![CONFIDENCE_BAR],
    }]
}

#[test]
fn the_recorded_run_looks_entirely_healthy() {
    let (trace, cas, did) = record(Gate::OnConfidence);
    assert_eq!(did, "deleted");

    let invariants: Vec<Box<dyn Invariant>> = vec![
        Box::new(NoDuplicateEffects::all()),
        Box::new(TerminatesWithin(24)),
    ];
    assert!(
        check_all(&trace, &cas, &invariants).is_empty(),
        "every call succeeded and nothing repeated — which is the problem"
    );
}

#[test]
fn a_destructive_gate_is_caught_sitting_on_the_threshold() {
    let (trace, mut cas, _) = record(Gate::OnConfidence);

    let findings = search_boundaries(&trace, &mut cas, &bar(), |r| {
        run_classifier(r, Gate::OnConfidence);
    });

    assert!(!findings.is_empty(), "the gate must be found");
    let finding = &findings[0];
    println!("{}", finding.report());

    assert_eq!(finding.field, "confidence");
    assert_eq!(finding.threshold, CONFIDENCE_BAR);
    assert_eq!(
        finding.recorded, 0.86,
        "what the recording actually returned"
    );
    assert!(
        finding.effectful,
        "a delete flips across the boundary — this is the severity signal"
    );
    assert!(
        finding.below_did.contains("delete_file") || finding.above_did.contains("delete_file"),
        "{finding:?}"
    );
}

#[test]
fn the_margin_makes_the_delete_insensitive_to_the_boundary() {
    // The fix is not a better threshold. It is refusing to act alone inside
    // a band where the score cannot support the decision.
    let (trace, mut cas, did) = record(Gate::WithMargin);
    assert_eq!(did, "deleted", "0.86 clears the bar with margin to spare");

    let findings = search_boundaries(&trace, &mut cas, &bar(), |r| {
        run_classifier(r, Gate::WithMargin);
    });

    assert!(
        findings.iter().all(|f| !f.effectful),
        "no destructive call may flip on a hair: {:?}",
        findings.iter().map(|f| f.report()).collect::<Vec<_>>()
    );
}

#[test]
fn probing_the_threshold_exactly_separates_gt_from_gte() {
    // `>` versus `>=` is the whole bug in a meaningful share of gates, and it
    // is invisible unless the exact value is one of the probes.
    let (trace, mut cas, _) = record(Gate::OnConfidence);

    let findings = search_boundaries(&trace, &mut cas, &bar(), |r| {
        run_classifier(r, Gate::OnConfidence);
    });

    let touches_exact = findings
        .iter()
        .any(|f| f.below == CONFIDENCE_BAR || f.above == CONFIDENCE_BAR);
    assert!(
        touches_exact,
        "the exact threshold must be probed: {findings:?}"
    );
}

#[test]
fn a_threshold_nobody_branches_on_reports_nothing() {
    // A detector that fires on every declared number would be noise.
    let (trace, mut cas, _) = record(Gate::OnConfidence);

    let unused = vec![BoundarySpec {
        field: "confidence".into(),
        thresholds: vec![0.2],
    }];
    let findings = search_boundaries(&trace, &mut cas, &unused, |r| {
        run_classifier(r, Gate::OnConfidence);
    });
    assert!(
        findings.is_empty(),
        "0.2 is nowhere near the gate: {findings:?}"
    );
}

#[test]
fn a_field_no_response_carries_is_left_alone() {
    let (trace, mut cas, _) = record(Gate::OnConfidence);
    let absent = vec![BoundarySpec {
        field: "certainty".into(),
        thresholds: vec![0.8],
    }];
    assert!(search_boundaries(&trace, &mut cas, &absent, |r| {
        run_classifier(r, Gate::OnConfidence);
    })
    .is_empty());
}

#[test]
fn the_fault_moves_the_number_without_inventing_one() {
    use samsara_core::fault::{read_path, set_path};
    use serde_json::json;

    let mut body = json!({"answer": "yes", "confidence": 0.86, "meta": {"score": 1.0}});
    assert!(set_path(&mut body, "confidence", json!(0.79)));
    assert_eq!(read_path(&body, "confidence"), Some(0.79));

    assert!(set_path(&mut body, "meta.score", json!(0.5)));
    assert_eq!(read_path(&body, "meta.score"), Some(0.5));

    // Creating a key that was never returned would test a response shape
    // that cannot occur.
    assert!(!set_path(&mut body, "certainty", json!(0.5)));
    assert!(!set_path(&mut body, "meta.missing.deep", json!(0.5)));
    assert_eq!(read_path(&body, "certainty"), None);
}
