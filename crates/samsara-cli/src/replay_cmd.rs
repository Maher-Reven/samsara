//! Replaying a recorded trace back into a live agent process.
//!
//! Recording answers "what did my agent do". This answers the two questions
//! that actually cost people time:
//!
//! - **Did I change anything?** Strict replay feeds the agent its recorded
//!   answers and asserts it asks the same questions in the same order. Edit a
//!   prompt, reorder a tool list, upgrade an SDK, then run this: any
//!   behavioural change shows up as a located divergence rather than as a
//!   vague feeling that something is different.
//! - **Does it survive adversity?** Counterfactual replay injects a fault
//!   schedule generated from a seed, so `--seed 91238` reproduces a specific
//!   production failure against your real agent, offline.
//!
//! Neither costs a token, because no request leaves the machine.
//!
//! # How it attaches
//!
//! The agent is a subprocess talking HTTP, not a Rust function — but
//! [`Replayer::perform`] does not care who calls it. The HTTP handler builds
//! an `EffectRequest` from the incoming request and asks the replayer, which
//! is exactly what the in-process harness does. That is why the engine needed
//! no changes to support this.
//!
//! The one wrinkle: a `Replayer` borrows its trace and its object store, and
//! a borrow cannot live in a `Mutex` shared across request threads. Both are
//! process-lifetime data, so they are leaked deliberately — `Box::leak` is
//! the honest way to say "this lives until the process exits", and costs one
//! allocation that we were never going to free anyway.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use samsara_core::prelude::*;
use serde_json::{json, Value};

use crate::record::{json_response, now_ms};
use crate::ui;

/// What the agent is replayed against.
pub struct Options {
    pub trace: PathBuf,
    pub out: Option<PathBuf>,
    pub port: u16,
    /// Generate a schedule from this seed.
    pub seed: Option<u64>,
    pub max_faults: usize,
    /// Fail on any divergence, and inject nothing.
    pub strict: bool,
    /// Tools whose effects must not happen twice.
    pub effectful: Vec<String>,
    pub idempotency_key: Option<String>,
}

/// A burst of concurrent tool calls, assembling.
struct Batch {
    /// How many calls the recording says belong here.
    expected: usize,
    /// Calls that have arrived, in arrival order — which is meaningless and
    /// deliberately unused for anything but bookkeeping.
    arrived: Vec<(String, EffectRequest)>,
    /// Outcomes, once the batch has been scheduled.
    outcomes: HashMap<String, Outcome>,
    /// Call ids in the completion order the scheduler chose.
    release: Vec<String>,
    /// How many have been answered. A caller may only respond when the
    /// cursor points at it.
    ///
    /// Responses are handed back one at a time, and the next is not sent
    /// until the previous has been written to its socket. Releasing them all
    /// at once would leave the agent to observe them in whatever order its
    /// own scheduler produced, which is the race we are here to remove.
    cursor: usize,
    scheduled: bool,
}

struct Session {
    replayer: Option<Replayer<'static, FsCas>>,
    /// Batches in flight, keyed by the id the shim reported.
    batches: HashMap<u64, Batch>,
    /// Shim batch ids in order of first appearance, so they can be lined up
    /// with the recording's batches positionally rather than by value.
    seen: Vec<u64>,
    /// Member identities of each concurrent batch in the recording, in the
    /// order the recording performed them.
    ///
    /// Arrival order is a race and must never reach the scheduler. Sorting
    /// arrivals into this order first is what makes a seed reproduce: the
    /// permutation is then applied to a fixed list rather than to whatever
    /// order three processes happened to win in.
    recorded: Vec<Vec<Digest>>,
    /// The identity policy the recording used.
    canon: Canonicalizer,
    next_call: u64,
}

type Shared = Arc<(Mutex<Session>, Condvar)>;

/// How long a batch waits for its remaining members before giving up.
///
/// This is the one place Samsara can hang, and it is worth being honest
/// about why. To reorder concurrent calls we must hold the early arrivals
/// until we know what they are being reordered against — but a counterfactual
/// agent may have diverged and may never issue the calls we are waiting for.
/// So the wait is bounded, and on expiry we schedule whatever turned up. The
/// alternative is a tool that deadlocks on exactly the runs it exists to
/// investigate.
const BATCH_TIMEOUT: Duration = Duration::from_millis(2000);

/// A running replay endpoint that can be driven repeatedly.
///
/// A sweep needs the agent run once per schedule -- hundreds of times. The
/// server is started once and reused; only the replayer is swapped between
/// runs. Standing up a fresh listener each time would churn several hundred
/// ports and race with anything else on the machine.
pub struct Harness {
    shared: Shared,
    base: String,
    objects: PathBuf,
    trace: &'static Trace,
    stop: Arc<AtomicU64>,
    serving: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    /// Start the endpoint for `trace`.
    pub fn start(
        trace_path: &std::path::Path,
        port: u16,
    ) -> Result<Harness, Box<dyn std::error::Error>> {
        let file = std::fs::File::open(trace_path)
            .map_err(|e| format!("cannot open {}: {e}", trace_path.display()))?;
        let trace = Trace::read(std::io::BufReader::new(file))?;
        let objects = crate::objects_dir(trace_path);

        let (recorded, canon) = batch_layout(&trace);
        let trace: &'static Trace = Box::leak(Box::new(trace));

        let shared: Shared = Arc::new((
            Mutex::new(Session {
                replayer: None,
                batches: HashMap::new(),
                seen: Vec::new(),
                recorded,
                canon,
                next_call: 0,
            }),
            Condvar::new(),
        ));

        let server = tiny_http::Server::http(("127.0.0.1", port))
            .map_err(|e| format!("cannot bind 127.0.0.1:{port}: {e}"))?;
        let stop = Arc::new(AtomicU64::new(0));
        let serving = spawn_server(server, Arc::clone(&shared), Arc::clone(&stop));

        Ok(Harness {
            shared,
            base: format!("http://127.0.0.1:{port}"),
            objects,
            trace,
            stop,
            serving: Some(serving),
        })
    }

    pub fn trace(&self) -> &'static Trace {
        self.trace
    }

    /// Run the agent once under `mode`.
    pub fn run_once(
        &self,
        mode: Mode,
        command: &[String],
    ) -> Result<(ReplayResult, i32), Box<dyn std::error::Error>> {
        // A fresh store handle per run. `FsCas` is a path, so leaking one
        // per schedule costs a few hundred bytes across a whole sweep and
        // avoids threading a borrow back out of a consumed replayer.
        let cas: &'static mut FsCas = Box::leak(Box::new(FsCas::open(&self.objects)?));
        let replayer = Replayer::new(self.trace, cas, mode)?;

        {
            let mut session = self.shared.0.lock().unwrap();
            session.replayer = Some(replayer);
            // Per-run state must not survive into the next schedule, or a
            // batch from run seven would still be assembling in run eight.
            session.batches.clear();
            session.seen.clear();
            session.next_call = 0;
        }

        let status = std::process::Command::new(&command[0])
            .args(&command[1..])
            .env("ANTHROPIC_BASE_URL", &self.base)
            .env("OPENAI_BASE_URL", format!("{}/v1", self.base))
            .env("SAMSARA_ENDPOINT", format!("{}/_samsara", self.base))
            .env("ANTHROPIC_API_KEY", "samsara-replay-no-live-calls")
            .env("OPENAI_API_KEY", "samsara-replay-no-live-calls")
            .status()
            .map_err(|e| format!("cannot run `{}`: {e}", command[0]))?;

        let replayer = self
            .shared
            .0
            .lock()
            .unwrap()
            .replayer
            .take()
            .expect("a replayer was installed for this run");

        Ok((replayer.finish("replay"), status.code().unwrap_or(-1)))
    }

    pub fn shutdown(mut self) {
        self.stop.store(1, Ordering::Relaxed);
        let _ = ureq::get(&format!("{}/_samsara/ping", self.base))
            .timeout(std::time::Duration::from_millis(250))
            .call();
        if let Some(handle) = self.serving.take() {
            let _ = handle.join();
        }
    }
}

/// Batch membership from a recording, in order of appearance.
fn batch_layout(trace: &Trace) -> (Vec<Vec<Digest>>, Canonicalizer) {
    let mut recorded: Vec<Vec<Digest>> = Vec::new();
    let mut seen: Vec<u64> = Vec::new();
    for event in &trace.events {
        if let Some(batch) = event.batch {
            match seen.iter().position(|b| *b == batch) {
                Some(i) => recorded[i].push(event.identity.clone()),
                None => {
                    seen.push(batch);
                    recorded.push(vec![event.identity.clone()]);
                }
            }
        }
    }
    (recorded, trace.header.canonicalizer.clone())
}

fn spawn_server(
    server: tiny_http::Server,
    shared: Shared,
    stop: Arc<AtomicU64>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        // Each request is handled on its own thread, and it has to be.
        //
        // A barrier holds early arrivals until the rest of their burst turns
        // up -- so if the accept loop handled requests one at a time, the
        // first call would block the loop against the very calls it is
        // waiting for. The bound on the wait would turn that deadlock into a
        // slow timeout, which is worse than a crash: it looks like it works.
        let mut workers = Vec::new();
        for request in server.incoming_requests() {
            if stop.load(Ordering::Relaxed) == 1 {
                break;
            }
            let shared = Arc::clone(&shared);
            workers.push(std::thread::spawn(move || {
                if let Err(e) = handle(request, &shared) {
                    eprintln!("{} {e}", ui::yellow("replay:"));
                }
            }));
            workers.retain(|w| !w.is_finished());
        }
        for worker in workers {
            let _ = worker.join();
        }
    })
}

pub fn run(options: Options, command: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if command.is_empty() {
        return Err("nothing to run: pass the agent command after `--`".into());
    }

    let harness = Harness::start(&options.trace, options.port)?;
    let trace = harness.trace();

    let mode = if options.strict {
        Mode::Strict
    } else {
        let schedule = match options.seed {
            Some(seed) => FaultSchedule::generate(seed, &trace.faultable(), options.max_faults),
            None => FaultSchedule::empty(),
        };
        Mode::Counterfactual { schedule }
    };

    match &mode {
        Mode::Strict => ui::info("strict replay: any divergence is an error"),
        Mode::Counterfactual { schedule } if schedule.is_empty() => {
            ui::info("replaying with no faults")
        }
        Mode::Counterfactual { schedule } => {
            ui::info(&format!("injecting {}", ui::yellow(&schedule.describe())))
        }
    }
    ui::ok(&format!(
        "replaying {} ({} effects)",
        options.trace.display(),
        trace.len()
    ));

    let (result, child_exit) = harness.run_once(mode, command)?;
    harness.shutdown();
    report(result, &options, child_exit)
}

fn report(
    result: ReplayResult,
    options: &Options,
    child_exit: i32,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut failed = false;

    if result.divergences.is_empty() {
        ui::ok("no divergence: the agent asked exactly what it asked before");
    } else {
        // Only the first matters; everything after is downstream of it.
        let first = &result.divergences[0];
        let extra = result.divergences.len() - 1;
        let line = if extra > 0 {
            format!("{} (+{extra} more, all downstream)", first.report())
        } else {
            first.report()
        };
        if options.strict {
            ui::bad(&line);
            failed = true;
        } else {
            // After a fault, divergence is the point.
            ui::info(&line);
        }
    }

    if !result.holes.is_empty() {
        ui::info(&format!(
            "{} effect(s) had no recorded answer; re-record to cover that path",
            result.holes.len()
        ));
    }

    // Check the branch, which is where an injected fault shows its damage.
    let mut duplicates = if options.effectful.is_empty() {
        NoDuplicateEffects::all()
    } else {
        NoDuplicateEffects::only(options.effectful.clone())
    };
    if let Some(field) = &options.idempotency_key {
        duplicates = duplicates.exempting_idempotent(field.clone());
    }
    let invariants: Vec<Box<dyn Invariant>> =
        vec![Box::new(duplicates), Box::new(TerminatesWithin(512))];

    let cas = FsCas::open(crate::objects_dir(&options.trace))?;
    let violations = check_all(&result.branch, &cas, &invariants);
    for violation in &violations {
        ui::bad(&violation.report());
        failed = true;
    }
    if violations.is_empty() {
        ui::ok(&format!("{} effects, no violations", result.branch.len()));
    }

    if let Some(out) = &options.out {
        let mut header = result.branch.header.clone();
        header.recorded_at_ms = now_ms();
        let branch = Trace {
            header,
            events: result.branch.events.clone(),
        };
        std::fs::write(out, branch.to_jsonl())?;
        ui::info(&format!("branch written to {}", out.display()));
    }

    if child_exit != 0 {
        ui::info(&format!("agent exited {child_exit}"));
    }

    // Non-zero on failure so this works as a regression test in CI.
    if failed {
        std::process::exit(1);
    }
    Ok(())
}

fn handle(
    mut request: tiny_http::Request,
    shared: &Shared,
) -> Result<(), Box<dyn std::error::Error>> {
    let url = request.url().to_string();
    let mut body = Vec::new();
    request.as_reader().read_to_end(&mut body)?;

    let response = match url.strip_prefix("/_samsara") {
        Some("/ping") => json_response(200, json!({"ok": true})),
        Some("/begin") => {
            let incoming: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let effect = EffectRequest {
                kind: crate::record::effect_kind(&incoming),
                name: incoming
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                body: incoming.get("body").cloned().unwrap_or(Value::Null),
            };
            let batch = incoming.get("batch").and_then(|v| v.as_u64());
            let (call, outcome) = perform_tool(shared, effect, batch);
            // Always `return`: during replay the real tool must never run.
            // This is the branch the recording proxy does not emit, and the
            // reason the shim has one.
            let response = json_response(
                200,
                json!({"action": "return", "call": call, "outcome": outcome}),
            );
            // Respond here rather than at the bottom, so the next call in
            // the batch is not released until this one is on the wire.
            request.respond(response)?;
            if let Some(batch_id) = batch {
                advance(shared, batch_id);
            }
            return Ok(());
        }
        Some("/end") => {
            // The shim returns before reaching /end during replay. A client
            // that calls it anyway is answered rather than failed, but the
            // outcome it reports is discarded: it did not really happen.
            json_response(200, json!({"outcome": {"status": "ok", "value": null}}))
        }
        Some(other) => json_response(404, json!({"error": format!("no such endpoint: {other}")})),
        None => {
            let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let model = parsed
                .get("model")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let outcome = perform(shared, EffectRequest::model(model, parsed));
            match outcome {
                Outcome::Ok { value } => json_response(200, value),
                Outcome::Err { code, message } => {
                    // A recorded provider error is replayed as that error.
                    // Note that an injected `timeout` surfaces as a status
                    // rather than as latency: we do not make the caller wait,
                    // which means a bug that depends on wall-clock timeout
                    // handling rather than on the error itself is out of
                    // reach here.
                    let status = code.parse::<u16>().unwrap_or(504);
                    json_response(status, json!({"error": {"type": code, "message": message}}))
                }
            }
        }
    };

    request.respond(response)?;
    Ok(())
}

fn perform(shared: &Shared, effect: EffectRequest) -> Outcome {
    let mut guard = shared.0.lock().unwrap();
    match guard.replayer.as_mut() {
        Some(replayer) => replayer.perform(effect),
        None => Outcome::err("samsara_finished", "replay session already closed"),
    }
}

/// Resolve a tool call, holding it at the barrier if it belongs to a
/// concurrent batch.
///
/// Returns the call id alongside the outcome so the shim can correlate.
fn perform_tool(shared: &Shared, effect: EffectRequest, batch: Option<u64>) -> (String, Outcome) {
    let (lock, cvar) = &**shared;
    let mut session = lock.lock().unwrap();

    let call = format!("c{}", session.next_call);
    session.next_call += 1;

    // Which recorded batch does this one line up with? Positional, not by
    // value: the shim's counter restarts every run and need not agree with
    // whatever the recording happened to use.
    let Some(batch_id) = batch else {
        let outcome = perform_locked(&mut session, effect);
        return (call, outcome);
    };
    let index = match session.seen.iter().position(|b| *b == batch_id) {
        Some(i) => i,
        None => {
            session.seen.push(batch_id);
            session.seen.len() - 1
        }
    };
    let expected = session.recorded.get(index).map_or(1, Vec::len);

    // A batch of one needs no barrier and must not be tagged as concurrent.
    if expected <= 1 {
        let outcome = perform_locked(&mut session, effect);
        return (call, outcome);
    }

    session
        .batches
        .entry(batch_id)
        .or_insert_with(|| Batch {
            expected,
            arrived: Vec::new(),
            outcomes: HashMap::new(),
            release: Vec::new(),
            cursor: 0,
            scheduled: false,
        })
        .arrived
        .push((call.clone(), effect));

    let complete = {
        let b = &session.batches[&batch_id];
        b.arrived.len() >= b.expected
    };

    if complete {
        schedule_batch(&mut session, batch_id);
        cvar.notify_all();
    } else {
        // Wait for the rest of the burst, but never forever.
        let deadline = std::time::Instant::now() + BATCH_TIMEOUT;
        while !session.batches.get(&batch_id).is_some_and(|b| b.scheduled) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                ui::info(&format!(
                    "batch {batch_id} timed out with {}/{expected} calls;                      scheduling what arrived",
                    session.batches.get(&batch_id).map_or(0, |b| b.arrived.len())
                ));
                schedule_batch(&mut session, batch_id);
                cvar.notify_all();
                break;
            }
            let (guard, _) = cvar.wait_timeout(session, remaining).unwrap();
            session = guard;
        }
    }

    // Wait for this call's turn to be answered.
    loop {
        let my_turn = session
            .batches
            .get(&batch_id)
            .is_none_or(|b| b.release.get(b.cursor).is_none_or(|next| *next == call));
        if my_turn {
            break;
        }
        let (guard, _) = cvar.wait_timeout(session, BATCH_TIMEOUT).unwrap();
        session = guard;
    }

    let outcome = session
        .batches
        .get_mut(&batch_id)
        .and_then(|b| b.outcomes.remove(&call))
        .unwrap_or_else(|| {
            Outcome::err(
                "samsara_unscheduled",
                "call was not scheduled with its batch",
            )
        });

    (call, outcome)
}

/// Let the next call in a batch be answered.
///
/// Called only after the current response has been written, so the agent
/// observes results in the order the scheduler chose rather than in whatever
/// order its own runtime wakes up.
fn advance(shared: &Shared, batch_id: u64) {
    let (lock, cvar) = &**shared;
    let mut session = lock.lock().unwrap();
    let finished = match session.batches.get_mut(&batch_id) {
        Some(batch) => {
            batch.cursor += 1;
            batch.cursor >= batch.release.len()
        }
        None => false,
    };
    if finished {
        session.batches.remove(&batch_id);
    }
    drop(session);
    cvar.notify_all();
}

/// Run a batch's calls through the engine's scheduler, which decides the
/// completion order and applies any scheduling fault.
fn schedule_batch(session: &mut Session, batch_id: u64) {
    let Some(batch) = session.batches.get_mut(&batch_id) else {
        return;
    };
    if batch.scheduled {
        return;
    }
    let mut arrived = std::mem::take(&mut batch.arrived);
    batch.scheduled = true;

    // Put the batch into the order the recording performed it in, *before*
    // anything is scheduled.
    //
    // The list arrived in race order. Permuting a race gives a different
    // answer every run, which would make a seed unreproducible — the one
    // thing this project cannot get wrong. So each arrival is matched to its
    // slot in the recorded batch by effect identity, and anything unmatched
    // (an agent that diverged and called something new) is appended in a
    // stable order rather than dropped.
    let index = session.seen.iter().position(|b| *b == batch_id);
    let expected: &[Digest] = index
        .and_then(|i| session.recorded.get(i))
        .map(Vec::as_slice)
        .unwrap_or(&[]);

    let canon = session.canon.clone();
    let mut used = vec![false; expected.len()];
    let mut slotted: Vec<(usize, (String, EffectRequest))> = Vec::with_capacity(arrived.len());
    for entry in arrived.drain(..) {
        let identity = entry.1.identity(&canon);
        let slot = expected
            .iter()
            .enumerate()
            .find(|(i, id)| !used[*i] && **id == identity)
            .map(|(i, _)| i);
        match slot {
            Some(i) => {
                used[i] = true;
                slotted.push((i, entry));
            }
            // Unmatched calls sort after every recorded one, by name then
            // arguments, so the order is a function of the calls themselves.
            None => slotted.push((usize::MAX, entry)),
        }
    }
    slotted.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1 .1.name.cmp(&b.1 .1.name))
            .then_with(|| {
                a.1 .1
                    .identity(&canon)
                    .as_str()
                    .cmp(b.1 .1.identity(&canon).as_str())
            })
    });
    let arrived: Vec<(String, EffectRequest)> = slotted.into_iter().map(|(_, e)| e).collect();

    let requests: Vec<EffectRequest> = arrived.iter().map(|(_, r)| r.clone()).collect();
    let results = match session.replayer.as_mut() {
        Some(replayer) => replayer.perform_batch(requests),
        None => arrived
            .iter()
            .enumerate()
            .map(|(i, _)| {
                (
                    i,
                    Outcome::err("samsara_finished", "replay session already closed"),
                )
            })
            .collect(),
    };

    let batch = session.batches.get_mut(&batch_id).expect("just inserted");
    for (index, outcome) in results {
        if let Some((call, _)) = arrived.get(index) {
            batch.outcomes.insert(call.clone(), outcome);
            // `results` come back in completion order, so this is the order
            // responses must be handed out in.
            batch.release.push(call.clone());
        }
    }
}

fn perform_locked(session: &mut Session, effect: EffectRequest) -> Outcome {
    match session.replayer.as_mut() {
        Some(replayer) => replayer.perform(effect),
        None => Outcome::err("samsara_finished", "replay session already closed"),
    }
}
