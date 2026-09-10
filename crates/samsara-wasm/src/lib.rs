//! WebAssembly bindings for the Samsara engine.
//!
//! The browser timeline is not a mock-up of the engine, it *is* the engine.
//! The same Rust that runs in CI is compiled to `wasm32-unknown-unknown` and
//! shipped to the page, so dragging a fault onto an effect performs a real
//! counterfactual replay, against the real oracle, checked by the real
//! invariants.
//!
//! That matters beyond neatness. A hand-written JavaScript reimplementation
//! would be a second source of truth, would drift within a month, and would
//! quietly make the demo lie about what the tool does.

use samsara_core::prelude::*;
use samsara_core::testkit::{run_agent, FakeBackend, Style};
use serde::Serialize;
use serde_json::Value;
use wasm_bindgen::prelude::*;

/// One effect, with its payloads inlined so the page needs no second lookup.
#[derive(Serialize)]
struct EventView {
    seq: u64,
    kind: String,
    name: String,
    identity: String,
    /// Short digest, for display.
    id_short: String,
    request: Value,
    outcome: Value,
    /// Present only where a fault was injected: what the agent was prevented
    /// from seeing.
    shadow: Option<Value>,
    fault: Option<String>,
    logical_time: u64,
}

#[derive(Serialize)]
struct ViolationView {
    invariant: String,
    at_seq: Option<u64>,
    detail: String,
}

#[derive(Serialize)]
struct RunView {
    events: Vec<EventView>,
    violations: Vec<ViolationView>,
    holes: usize,
    /// Positions a fault can usefully be attached to.
    faultable: Vec<u64>,
}

#[derive(Serialize)]
struct FindingView {
    seed: u64,
    schedule: FaultSchedule,
    description: String,
    violations: Vec<ViolationView>,
    /// The minimised schedule, and what it cost to find.
    shrunk: FaultSchedule,
    shrunk_description: String,
    evaluations: usize,
    started_with: usize,
}

/// A loaded recording, ready to be forked.
#[wasm_bindgen]
pub struct Session {
    trace: Trace,
    cas: MemCas,
    style: Style,
}

fn payload(cas: &MemCas, digest: &Digest) -> Value {
    cas.get(digest)
        .ok()
        .flatten()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or(Value::Null)
}

fn view(trace: &Trace, cas: &MemCas, violations: Vec<Violation>, holes: usize) -> RunView {
    RunView {
        events: trace
            .events
            .iter()
            .map(|e| EventView {
                seq: e.seq,
                kind: e.kind.as_str().to_string(),
                name: e.name.clone(),
                identity: e.identity.to_string(),
                id_short: e.identity.short().to_string(),
                request: payload(cas, &e.request),
                outcome: payload(cas, &e.outcome),
                shadow: e.shadow.as_ref().map(|d| payload(cas, d)),
                fault: e.fault.as_ref().map(|f| f.label()),
                logical_time: e.logical_time,
            })
            .collect(),
        violations: violations
            .into_iter()
            .map(|v| ViolationView {
                invariant: v.invariant,
                at_seq: v.at_seq,
                detail: v.detail,
            })
            .collect(),
        holes,
        faultable: trace.faultable(),
    }
}

fn invariants() -> Vec<Box<dyn Invariant>> {
    vec![
        Box::new(NoDuplicateEffects::only(["delete_file"]).exempting_idempotent("idempotency_key")),
        Box::new(TerminatesWithin(24)),
    ]
}

#[wasm_bindgen]
impl Session {
    /// Record the built-in demo agent and return a session over it.
    ///
    /// `style` is `"naive"` (the agent with the retry bug) or `"idempotent"`
    /// (the fixed one), so the page can show both against the same faults.
    #[wasm_bindgen(constructor)]
    pub fn new(style: &str) -> Session {
        let style = if style == "idempotent" {
            Style::Idempotent
        } else {
            Style::Naive
        };
        let mut cas = MemCas::new();
        let mut recorder = Recorder::new(FakeBackend::new(1), &mut cas, Canonicalizer::default());
        run_agent(&mut recorder, style);
        let trace = recorder.finish("delete-stale-report");
        Session { trace, cas, style }
    }

    /// The clean recording, checked against the invariants.
    #[wasm_bindgen]
    pub fn recording(&self) -> String {
        let violations = check_all(&self.trace, &self.cas, &invariants());
        json(&view(&self.trace, &self.cas, violations, 0))
    }

    /// Replay under a fault schedule and return the resulting timeline.
    ///
    /// `schedule` is JSON: `{"points":[{"seq":1,"fault":{"type":"timeout"}}]}`.
    #[wasm_bindgen]
    pub fn fork(&mut self, schedule: &str) -> String {
        let schedule: FaultSchedule = match serde_json::from_str(schedule) {
            Ok(s) => s,
            Err(e) => return json(&serde_json::json!({ "error": e.to_string() })),
        };

        let (branch, holes) = {
            let mut replayer = match Replayer::new(
                &self.trace,
                &mut self.cas,
                Mode::Counterfactual { schedule },
            ) {
                Ok(r) => r,
                Err(e) => return json(&serde_json::json!({ "error": e.to_string() })),
            };
            run_agent(&mut replayer, self.style);
            let result = replayer.finish("counterfactual");
            (result.branch, result.holes.len())
        };

        let violations = check_all(&branch, &self.cas, &invariants());
        json(&view(&branch, &self.cas, violations, holes))
    }

    /// Search seeds for a schedule that breaks an invariant, then minimise
    /// it. Returns `null` if the agent survived every seed tried.
    #[wasm_bindgen]
    pub fn search(&mut self, seeds: u32, max_faults: usize) -> String {
        let style = self.style;
        let trace = self.trace.clone();

        let Some(finding) = samsara_core::explore::search(
            &trace,
            &mut self.cas,
            0..seeds as u64,
            max_faults,
            &invariants(),
            |r| {
                run_agent(r, style);
            },
        ) else {
            return "null".to_string();
        };

        let shrunk =
            samsara_core::explore::minimise(&trace, &mut self.cas, &finding, &invariants(), |r| {
                run_agent(r, style);
            });

        json(&FindingView {
            seed: finding.seed,
            description: finding.schedule.describe(),
            schedule: finding.schedule,
            violations: finding
                .violations
                .into_iter()
                .map(|v| ViolationView {
                    invariant: v.invariant,
                    at_seq: v.at_seq,
                    detail: v.detail,
                })
                .collect(),
            shrunk_description: shrunk.schedule.describe(),
            shrunk: shrunk.schedule,
            evaluations: shrunk.evaluations,
            started_with: shrunk.started_with,
        })
    }

    /// Generate the schedule a seed produces, without running it.
    #[wasm_bindgen]
    pub fn schedule_for_seed(&self, seed: u64, max_faults: usize) -> String {
        json(&FaultSchedule::generate(
            seed,
            &self.trace.faultable(),
            max_faults,
        ))
    }
}

fn json<T: Serialize>(value: &T) -> String {
    serde_json::to_string(value).unwrap_or_else(|e| format!(r#"{{"error":{:?}}}"#, e.to_string()))
}

/// Engine version, so the page can show what it is actually running.
#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}
