//! A deterministic world and an agent to test in it.
//!
//! Everything Samsara claims has to be demonstrable without a network, an API
//! key, or a bill — otherwise the test suite is decorative and nobody runs it
//! in CI. So the model, the tools, the clock and the randomness all live here
//! as deterministic fakes, and the property tests drive real agent code
//! against them.
//!
//! The agent below is not a strawman. It is written the way agents are
//! actually written: ask the model what to do, call the tool, retry on
//! failure with jittered exponential backoff. That retry loop is the bug.

use std::collections::{HashMap, HashSet};

use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;
use serde_json::{json, Value};

use crate::event::{EffectKind, EffectRequest, Outcome};
use crate::replay::{Backend, Effects};

/// The state the tools act on. Records *physical* effects, so a test can
/// assert on what really happened rather than on what the agent believed.
#[derive(Debug, Default, Clone)]
pub struct World {
    /// Every delete that actually mutated state, in order. The ground truth
    /// the whole project exists to protect.
    pub deletes: Vec<String>,
    /// Idempotency keys already honoured.
    applied: HashSet<String>,
}

impl World {
    /// How many times the world was really mutated for `path`.
    pub fn delete_count(&self, path: &str) -> usize {
        self.deletes.iter().filter(|p| *p == path).count()
    }
}

/// A deterministic stand-in for the model API, the tools, and the clock.
#[derive(Debug)]
pub struct FakeBackend {
    pub world: World,
    rng: ChaCha8Rng,
    clock_ms: u64,
    /// Per-tool scripted failures: `name -> remaining failures to serve`.
    flaky: HashMap<String, usize>,
    /// Per-tool *lost responses*: the mutation happens, then the caller is
    /// told it timed out.
    lossy: HashMap<String, usize>,
}

impl FakeBackend {
    pub fn new(seed: u64) -> Self {
        FakeBackend {
            world: World::default(),
            rng: ChaCha8Rng::seed_from_u64(seed),
            clock_ms: 1_700_000_000_000,
            flaky: HashMap::new(),
            lossy: HashMap::new(),
        }
    }

    /// Make `tool` fail its first `times` calls, for tests that need a
    /// natural failure rather than an injected one. The call is refused
    /// outright: nothing is mutated.
    pub fn flaky(mut self, tool: &str, times: usize) -> Self {
        self.flaky.insert(tool.to_string(), times);
        self
    }

    /// Make `tool` *lose its response* on its first `times` calls: the work
    /// is done, the world is mutated, and then the caller is told it timed
    /// out.
    ///
    /// This is the failure that actually happens in production, and modelling
    /// it faithfully is how we check that Samsara's verdict corresponds to a
    /// genuine bug rather than an artefact of replay. A tool that merely
    /// refuses the call cannot produce a duplicate side effect, and a test
    /// built on one would prove nothing.
    pub fn lossy(mut self, tool: &str, times: usize) -> Self {
        self.lossy.insert(tool.to_string(), times);
        self
    }

    fn model(&mut self, body: &Value) -> Outcome {
        // The "model" is a lookup on the step field. Deterministic, free, and
        // sufficient: what we are testing is the agent's control flow, not
        // the model's judgement.
        let step = body.get("step").and_then(|v| v.as_str()).unwrap_or("");
        let content = match step {
            "plan" => json!({
                "tool": "delete_file",
                "arguments": {"path": "/var/reports/stale.csv"}
            }),
            _ => json!({"text": "Done. Removed the stale report."}),
        };
        Outcome::ok(json!({
            "content": content,
            "usage": {"input_tokens": 120, "output_tokens": 40}
        }))
    }

    fn tool(&mut self, name: &str, args: &Value) -> Outcome {
        if let Some(remaining) = self.flaky.get_mut(name) {
            if *remaining > 0 {
                *remaining -= 1;
                return Outcome::err("timeout", "samsara-testkit: scripted flake");
            }
        }

        // A lost response: perform the effect, then swallow the reply.
        let lose_response = match self.lossy.get_mut(name) {
            Some(remaining) if *remaining > 0 => {
                *remaining -= 1;
                true
            }
            _ => false,
        };

        let outcome = self.execute(name, args);
        if lose_response && outcome.is_ok() {
            return Outcome::err("timeout", "samsara-testkit: response lost in flight");
        }
        outcome
    }

    fn execute(&mut self, name: &str, args: &Value) -> Outcome {
        match name {
            "delete_file" => {
                let path = args
                    .get("path")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();

                // A tool given an idempotency key is obliged to honour it.
                // This is what a correctly built tool does, and it is why the
                // fixed agent is genuinely fixed rather than merely luckier.
                if let Some(key) = args.get("idempotency_key").and_then(|v| v.as_str()) {
                    if !self.applied_insert(key) {
                        return Outcome::ok(json!({"ok": true, "deduplicated": true}));
                    }
                }

                self.world.deletes.push(path.to_string());
                Outcome::ok(json!({"ok": true, "path": path}))
            }
            other => Outcome::err("unknown_tool", format!("no such tool: {other}")),
        }
    }

    /// Returns true if the key is new.
    fn applied_insert(&mut self, key: &str) -> bool {
        self.world.applied.insert(key.to_string())
    }
}

impl Backend for FakeBackend {
    fn call(&mut self, request: &EffectRequest) -> Outcome {
        match request.kind {
            EffectKind::Model => self.model(&request.body),
            EffectKind::Tool => {
                let name = request.name.clone();
                self.tool(&name, &request.body)
            }
            EffectKind::Clock => {
                // Time advances on every read, as it does in reality.
                self.clock_ms += 25;
                Outcome::ok(Value::from(self.clock_ms))
            }
            EffectKind::Random => Outcome::ok(Value::from(self.rng.gen::<f64>())),
        }
    }
}

// ---------------------------------------------------------------------------
// The agent under test
// ---------------------------------------------------------------------------

/// How the agent handles retries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Style {
    /// Retries a failed tool call by simply calling it again. This is what
    /// most agent code does, and it is wrong for any tool that mutates
    /// something: a timeout means *unknown*, not *didn't happen*.
    Naive,
    /// Sends an idempotency key that is stable across attempts, so a retry
    /// the tool has already honoured is deduplicated server-side.
    Idempotent,
}

/// What the agent did.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    /// Tool attempts made, including retries.
    pub attempts: usize,
    /// Whether the agent believes it succeeded.
    pub succeeded: bool,
}

/// Maximum tool attempts before giving up.
pub const MAX_ATTEMPTS: usize = 3;

/// Convenience for tests: reach the world through a recorder.
pub trait WorldAccess {
    fn world(&self) -> &World;
}

impl<C: crate::cas::Cas> WorldAccess for crate::replay::Recorder<'_, C, FakeBackend> {
    fn world(&self) -> &World {
        &self.backend().world
    }
}

/// Run the agent against any [`Effects`] implementation — the recorder in
/// production, the replayer in a counterfactual.
///
/// The shape is ordinary on purpose: plan, act, retry with jittered backoff,
/// summarise. The jitter is what makes this worth recording; a backoff
/// computed from a real clock and a real RNG is not reproducible, and every
/// retry bug hides behind exactly that.
pub fn run_agent<E: Effects>(fx: &mut E, style: Style) -> Report {
    // 1. Ask the model what to do.
    let plan = fx.perform(EffectRequest::model(
        "claude-sonnet-4",
        json!({"step": "plan", "task": "remove the stale report"}),
    ));

    let path = plan
        .value()
        .and_then(|v| v.pointer("/content/arguments/path"))
        .and_then(|v| v.as_str())
        .unwrap_or("/var/reports/stale.csv")
        .to_string();

    // A key derived from the task, not the attempt — the distinction that
    // makes it work. Deriving it per attempt would be the same bug wearing a
    // hat.
    let idempotency_key = format!("delete:{path}");

    // 2. Act, retrying on failure.
    let mut attempts = 0;
    let mut succeeded = false;

    while attempts < MAX_ATTEMPTS {
        attempts += 1;

        let mut args = json!({"path": path});
        if style == Style::Idempotent {
            args["idempotency_key"] = json!(idempotency_key);
        }

        let outcome = fx.perform(EffectRequest::tool("delete_file", args));
        if outcome.is_ok() {
            succeeded = true;
            break;
        }

        // Jittered exponential backoff. Reads the clock and the RNG, both of
        // which are effects, both of which are therefore reproducible.
        let jitter = fx.random();
        let base = 100u64 << (attempts - 1);
        let _wake_at = fx.now_ms() + base + (jitter * base as f64) as u64;
    }

    // 3. Summarise.
    fx.perform(EffectRequest::model(
        "claude-sonnet-4",
        json!({"step": "summarise", "succeeded": succeeded}),
    ));

    Report {
        attempts,
        succeeded,
    }
}
