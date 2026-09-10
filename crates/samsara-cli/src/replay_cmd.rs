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

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

type Shared = Arc<Mutex<Option<Replayer<'static, FsCas>>>>;

pub fn run(options: Options, command: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    if command.is_empty() {
        return Err("nothing to run: pass the agent command after `--`".into());
    }

    let file = std::fs::File::open(&options.trace)
        .map_err(|e| format!("cannot open {}: {e}", options.trace.display()))?;
    let trace = Trace::read(std::io::BufReader::new(file))?;
    let cas = FsCas::open(crate::objects_dir(&options.trace))?;

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

    // Process-lifetime data, deliberately leaked so the replayer can be
    // `'static` and live behind a mutex. See the module docs.
    let trace: &'static Trace = Box::leak(Box::new(trace));
    let cas: &'static mut FsCas = Box::leak(Box::new(cas));

    let shared: Shared = Arc::new(Mutex::new(Some(Replayer::new(trace, cas, mode)?)));

    let server = tiny_http::Server::http(("127.0.0.1", options.port))
        .map_err(|e| format!("cannot bind 127.0.0.1:{}: {e}", options.port))?;
    let base = format!("http://127.0.0.1:{}", options.port);
    ui::ok(&format!(
        "replaying {} ({} effects) on {base}",
        options.trace.display(),
        trace.len()
    ));

    let stop = Arc::new(AtomicU64::new(0));
    let serving = {
        let shared = Arc::clone(&shared);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                if stop.load(Ordering::Relaxed) == 1 {
                    break;
                }
                if let Err(e) = handle(request, &shared) {
                    eprintln!("{} {e}", ui::yellow("replay:"));
                }
            }
        })
    };

    let status = std::process::Command::new(&command[0])
        .args(&command[1..])
        .env("ANTHROPIC_BASE_URL", &base)
        .env("OPENAI_BASE_URL", format!("{base}/v1"))
        .env("SAMSARA_ENDPOINT", format!("{base}/_samsara"))
        // A replayed run must never reach a provider. Blanking the key means
        // a code path we failed to intercept fails loudly instead of quietly
        // spending money.
        .env("ANTHROPIC_API_KEY", "samsara-replay-no-live-calls")
        .env("OPENAI_API_KEY", "samsara-replay-no-live-calls")
        .status()
        .map_err(|e| format!("cannot run `{}`: {e}", command[0]))?;

    stop.store(1, Ordering::Relaxed);
    let _ = ureq::get(&format!("{base}/_samsara/ping"))
        .timeout(std::time::Duration::from_millis(250))
        .call();
    let _ = serving.join();

    let replayer = shared
        .lock()
        .unwrap()
        .take()
        .expect("replayer is taken once");
    report(
        replayer.finish("replay"),
        &options,
        status.code().unwrap_or(-1),
    )
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
            let effect = EffectRequest::tool(
                incoming
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
                incoming.get("body").cloned().unwrap_or(Value::Null),
            );
            let outcome = perform(shared, effect);
            // Always `return`: during replay the real tool must never run.
            // This is the branch the recording proxy does not emit, and the
            // reason the shim has one.
            json_response(200, json!({"action": "return", "outcome": outcome}))
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
    let mut guard = shared.lock().unwrap();
    match guard.as_mut() {
        Some(replayer) => replayer.perform(effect),
        None => Outcome::err("samsara_finished", "replay session already closed"),
    }
}
