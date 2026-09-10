//! Recording, replay, divergence detection, and counterfactual branching.
//!
//! # The two modes
//!
//! **Strict** replay asserts that the agent, fed its recorded answers, asks
//! exactly the same questions in exactly the same order. Any deviation is a
//! [`Divergence`] and is reported with the position and the reason. This is
//! the mode the metamorphic property test runs in: `replay(record(r)) == r`.
//!
//! **Counterfactual** replay is the interesting one. It replays faithfully up
//! to a fork point, injects a fault, and then lets the agent run free.
//! Divergence after the fork is not an error — it is the entire point.
//!
//! # Answering questions after the fork
//!
//! Once the agent has been lied to, it starts asking things the recording
//! never covered, and positional matching is finished. The obvious answers
//! are "call the real API again" (expensive, and nondeterministic, which
//! defeats the purpose) or "give up".
//!
//! Samsara does neither. It builds a [`ResponseOracle`] — an index from
//! *effect identity* to the outcomes that identity produced anywhere in the
//! recording — and serves post-fork effects from that.
//!
//! This is not a hack, it is the observation that makes offline
//! counterfactuals work at all: **a retry has the same identity as the call
//! it retries.** When we time out `delete_file(path=/x)` and the agent tries
//! again, the oracle already knows what `delete_file(path=/x)` returns,
//! because we watched it succeed thirty milliseconds ago. So we answer, the
//! agent proceeds happily — and the file has now been deleted twice. That is
//! the bug, reproduced offline, from a seed, with no API key.

use std::collections::HashMap;
use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use rand::Rng;
use rand::SeedableRng;
use rand_chacha::ChaCha8Rng;

use crate::canon::Canonicalizer;
use crate::cas::Cas;
use crate::event::{EffectKind, EffectRequest, Event, Outcome};
use crate::fault::{Fault, FaultSchedule};
use crate::hash::Digest;
use crate::trace::{Trace, TraceHeader};

/// The capability an agent under test is given. Every external interaction
/// goes through here; nothing else is observable.
pub trait Effects {
    /// Perform an effect and return what the agent observes.
    fn perform(&mut self, request: EffectRequest) -> Outcome;

    /// Wall-clock milliseconds. Recorded and replayed like any other effect.
    fn now_ms(&mut self) -> u64 {
        self.perform(EffectRequest::clock())
            .value()
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
    }

    /// A uniform draw in `[0, 1)`. Recorded and replayed like any other
    /// effect — which is what makes jittered retry backoff reproducible.
    fn random(&mut self) -> f64 {
        self.perform(EffectRequest::random())
            .value()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
    }
}

/// A source of real effects: the live model API, the real tools, the real
/// clock. Implemented by the proxy in production and by a deterministic fake
/// in tests.
pub trait Backend {
    fn call(&mut self, request: &EffectRequest) -> Outcome;
}

// ---------------------------------------------------------------------------
// Recording
// ---------------------------------------------------------------------------

/// Wraps a [`Backend`], writing every effect to a trace as it passes through.
pub struct Recorder<'a, C: Cas, B: Backend> {
    backend: B,
    cas: &'a mut C,
    canon: Canonicalizer,
    events: Vec<Event>,
    logical_time: u64,
}

impl<C: Cas, B: Backend> std::fmt::Debug for Recorder<'_, C, B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Recorder")
            .field("events", &self.events.len())
            .field("logical_time", &self.logical_time)
            .finish_non_exhaustive()
    }
}

impl<'a, C: Cas, B: Backend> Recorder<'a, C, B> {
    pub fn new(backend: B, cas: &'a mut C, canon: Canonicalizer) -> Self {
        Recorder {
            backend,
            cas,
            canon,
            events: Vec::new(),
            logical_time: 0,
        }
    }

    /// Borrow the backend, for tests that need to inspect the world the
    /// agent actually mutated.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    /// Finish recording and produce the trace.
    pub fn finish(self, label: impl Into<String>) -> Trace {
        let header = TraceHeader {
            label: label.into(),
            canonicalizer: self.canon,
            ..TraceHeader::default()
        };
        Trace {
            header,
            events: self.events,
        }
    }
}

impl<C: Cas, B: Backend> Effects for Recorder<'_, C, B> {
    fn perform(&mut self, request: EffectRequest) -> Outcome {
        let outcome = self.backend.call(&request);

        let identity = request.identity(&self.canon);
        let request_digest = self
            .cas
            .put(&serde_json::to_vec(&request).expect("request serialises"))
            .expect("cas write");
        let outcome_digest = self
            .cas
            .put(&serde_json::to_vec(&outcome).expect("outcome serialises"))
            .expect("cas write");

        self.events.push(Event {
            seq: self.events.len() as u64,
            kind: request.kind,
            name: request.name.clone(),
            identity,
            request: request_digest,
            outcome: outcome_digest,
            fault: None,
            shadow: None,
            logical_time: self.logical_time,
        });
        self.logical_time += 1;

        outcome
    }
}

// ---------------------------------------------------------------------------
// Divergence
// ---------------------------------------------------------------------------

/// Why a replay stopped matching its recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DivergenceKind {
    /// The agent performed an effect of a different sort than recorded.
    Kind {
        expected: EffectKind,
        found: EffectKind,
    },
    /// Same sort, different target — a different tool or a different model.
    Name { expected: String, found: String },
    /// Same target, different request. The common case, and the one worth
    /// rendering carefully.
    Identity {
        expected: Digest,
        found: Digest,
        /// First field at which the two requests differ, if we could isolate
        /// one. Enormously more useful than two hex strings.
        first_difference: Option<String>,
    },
    /// The agent kept going after the recording ran out.
    PastEnd { recorded: u64 },
    /// The agent stopped early.
    StoppedShort { recorded: u64, performed: u64 },
}

/// A divergence, located.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Divergence {
    pub at_seq: u64,
    #[serde(flatten)]
    pub kind: DivergenceKind,
}

impl Divergence {
    /// A report a human can act on.
    pub fn report(&self) -> String {
        match &self.kind {
            DivergenceKind::Kind { expected, found } => format!(
                "divergence at effect #{}: expected a {} effect, agent performed a {} effect",
                self.at_seq,
                expected.as_str(),
                found.as_str()
            ),
            DivergenceKind::Name { expected, found } => format!(
                "divergence at effect #{}: expected `{}`, agent called `{}`",
                self.at_seq, expected, found
            ),
            DivergenceKind::Identity {
                expected,
                found,
                first_difference,
            } => {
                let mut s = format!(
                    "divergence at effect #{}: same call, different arguments\n  recorded {}\n  replayed {}",
                    self.at_seq,
                    expected.short(),
                    found.short()
                );
                if let Some(d) = first_difference {
                    s.push_str(&format!("\n  first difference at {d}"));
                }
                s
            }
            DivergenceKind::PastEnd { recorded } => format!(
                "divergence at effect #{}: recording holds {} effects, agent wanted more",
                self.at_seq, recorded
            ),
            DivergenceKind::StoppedShort {
                recorded,
                performed,
            } => format!(
                "divergence: agent stopped after {performed} effects, recording holds {recorded}"
            ),
        }
    }
}

/// Locate the first differing field between two JSON documents, as a dotted
/// path. Returns `None` if they are equal.
///
/// This is what turns "two hashes differ" into "`messages.3.content` differs",
/// which is the difference between a usable tool and a frustrating one.
pub fn first_difference(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            // Union of keys, in a stable order, so the answer is deterministic.
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort_unstable();
            keys.dedup();
            for k in keys {
                let child = if path.is_empty() {
                    k.clone()
                } else {
                    format!("{path}.{k}")
                };
                match (x.get(k), y.get(k)) {
                    (Some(v1), Some(v2)) => {
                        if let Some(found) = first_difference(v1, v2, &child) {
                            return Some(found);
                        }
                    }
                    (Some(_), None) => return Some(format!("{child} (missing on replay)")),
                    (None, Some(_)) => return Some(format!("{child} (added on replay)")),
                    (None, None) => unreachable!("key came from one of the two maps"),
                }
            }
            None
        }
        (Value::Array(x), Value::Array(y)) => {
            for i in 0..x.len().max(y.len()) {
                let child = format!("{path}.{i}");
                match (x.get(i), y.get(i)) {
                    (Some(v1), Some(v2)) => {
                        if let Some(found) = first_difference(v1, v2, &child) {
                            return Some(found);
                        }
                    }
                    _ => return Some(format!("{child} (length {} vs {})", x.len(), y.len())),
                }
            }
            None
        }
        _ if a == b => None,
        _ => Some(if path.is_empty() {
            "(root)".into()
        } else {
            path.to_string()
        }),
    }
}

// ---------------------------------------------------------------------------
// The oracle
// ---------------------------------------------------------------------------

/// An index from effect identity to the outcomes that identity produced.
///
/// Post-fork, the agent asks questions the recording did not anticipate in
/// *position* but very often did in *content* — above all retries. The oracle
/// answers those without a network call.
#[derive(Debug, Default, Clone)]
pub struct ResponseOracle {
    /// Outcomes paired with the digest of their *original* bytes.
    ///
    /// Carrying the digest rather than recomputing it later is not an
    /// optimisation, it is a correctness requirement — see [`Replayer::emit`].
    by_identity: HashMap<Digest, Vec<(Digest, Outcome)>>,
    /// How many times each identity has been served, so repeated calls walk
    /// through the recorded outcomes rather than pinning to the first.
    served: HashMap<Digest, usize>,
}

impl ResponseOracle {
    /// Build from a trace, resolving outcome payloads out of the CAS.
    pub fn build<C: Cas>(trace: &Trace, cas: &C) -> io::Result<Self> {
        let mut by_identity: HashMap<Digest, Vec<(Digest, Outcome)>> = HashMap::new();
        for event in &trace.events {
            let Some(bytes) = cas.get(&event.outcome)? else {
                continue;
            };
            let Ok(outcome) = serde_json::from_slice::<Outcome>(&bytes) else {
                continue;
            };
            by_identity
                .entry(event.identity.clone())
                .or_default()
                .push((event.outcome.clone(), outcome));
        }
        Ok(ResponseOracle {
            by_identity,
            served: HashMap::new(),
        })
    }

    /// Answer an effect, if the recording ever saw this exact request.
    ///
    /// Repeated calls advance through the recorded outcomes and then hold on
    /// the last one. Holding rather than cycling matters: a tool that
    /// succeeded once should keep reporting success, not oscillate.
    pub fn answer(&mut self, identity: &Digest) -> Option<(Digest, Outcome)> {
        let outcomes = self.by_identity.get(identity)?;
        if outcomes.is_empty() {
            return None;
        }
        let n = self.served.entry(identity.clone()).or_insert(0);
        let idx = (*n).min(outcomes.len() - 1);
        *n += 1;
        Some(outcomes[idx].clone())
    }

    /// Distinct identities known.
    pub fn len(&self) -> usize {
        self.by_identity.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_identity.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Replay
// ---------------------------------------------------------------------------

/// How to replay.
#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// Every effect must match the recording. Used to prove replay is
    /// faithful.
    Strict,
    /// Replay faithfully, applying `schedule`, and tolerate divergence
    /// afterwards.
    Counterfactual { schedule: FaultSchedule },
}

/// An effect the replayer could not answer: not at this position, and not
/// anywhere in the recording.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hole {
    pub seq: u64,
    pub kind: EffectKind,
    pub name: String,
    pub identity: Digest,
}

/// Feeds an agent its recorded answers.
pub struct Replayer<'a, C: Cas> {
    trace: &'a Trace,
    cas: &'a mut C,
    canon: Canonicalizer,
    mode: Mode,
    oracle: ResponseOracle,

    /// Position in the recording while still in lockstep.
    cursor: usize,
    /// Set once we leave lockstep, either through a fault or a divergence.
    free_running: bool,

    divergences: Vec<Divergence>,
    holes: Vec<Hole>,
    branch: Vec<Event>,
    logical_time: u64,

    /// Clock and randomness once we are off-script.
    ///
    /// Post-fork these must not come from the oracle. Time has to keep
    /// advancing — an agent that reads the same millisecond forever will
    /// compute a zero backoff and spin — and randomness has to keep flowing.
    /// Both are derived from the parent trace's identity, so a counterfactual
    /// is still fully determined by (trace, schedule) with nothing left to
    /// chance.
    free_clock_ms: u64,
    free_rng: ChaCha8Rng,
}

/// How far the clock advances per read once we are off-script. Large enough
/// that successive reads are distinguishable, small enough to look like a
/// plausible agent step.
const FREE_CLOCK_TICK_MS: u64 = 25;

impl<C: Cas> std::fmt::Debug for Replayer<'_, C> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Replayer")
            .field("mode", &self.mode)
            .field("cursor", &self.cursor)
            .field("free_running", &self.free_running)
            .field("divergences", &self.divergences.len())
            .field("holes", &self.holes.len())
            .finish_non_exhaustive()
    }
}

impl<'a, C: Cas> Replayer<'a, C> {
    pub fn new(trace: &'a Trace, cas: &'a mut C, mode: Mode) -> io::Result<Self> {
        // Start the free-running clock after the last time the recording
        // observed, so a branch never appears to travel backwards.
        let last_clock = trace
            .events
            .iter()
            .filter(|e| e.kind == EffectKind::Clock)
            .filter_map(|e| cas.get(&e.outcome).ok().flatten())
            .filter_map(|b| serde_json::from_slice::<Outcome>(&b).ok())
            .filter_map(|o| o.value().and_then(|v| v.as_u64()))
            .max()
            .unwrap_or(0);

        // Seed from the parent's identity: deterministic, and different for
        // every trace, so two unrelated branches do not share a random walk.
        let id = trace.id();
        let mut seed_bytes = [0u8; 32];
        for (slot, byte) in seed_bytes.iter_mut().zip(id.as_str().bytes()) {
            *slot = byte;
        }

        Ok(Replayer {
            canon: trace.header.canonicalizer.clone(),
            oracle: ResponseOracle::build(trace, &*cas)?,
            free_clock_ms: last_clock,
            free_rng: ChaCha8Rng::from_seed(seed_bytes),
            trace,
            cas,
            mode,
            cursor: 0,
            free_running: false,
            divergences: Vec::new(),
            holes: Vec::new(),
            branch: Vec::new(),
            logical_time: 0,
        })
    }

    /// Divergences observed. In `Strict` mode a faithful replay leaves this
    /// empty; anything here is a bug in the agent, the shim, or Samsara.
    pub fn divergences(&self) -> &[Divergence] {
        &self.divergences
    }

    /// Effects the recording could not answer.
    pub fn holes(&self) -> &[Hole] {
        &self.holes
    }

    /// Whether replay stayed in lockstep with the recording throughout.
    pub fn is_faithful(&self) -> bool {
        self.divergences.is_empty() && self.holes.is_empty()
    }

    /// Finish, checking that the agent consumed the whole recording.
    ///
    /// Only meaningful in `Strict` mode: a counterfactual branch is *expected*
    /// to be a different length.
    pub fn finish(mut self, label: impl Into<String>) -> ReplayResult {
        if self.mode == Mode::Strict && self.cursor < self.trace.events.len() {
            self.divergences.push(Divergence {
                at_seq: self.cursor as u64,
                kind: DivergenceKind::StoppedShort {
                    recorded: self.trace.events.len() as u64,
                    performed: self.cursor as u64,
                },
            });
        }

        let (seed, schedule) = match &self.mode {
            Mode::Strict => (None, None),
            Mode::Counterfactual { schedule } => (None, Some(schedule.clone())),
        };

        let header = TraceHeader {
            label: label.into(),
            canonicalizer: self.canon.clone(),
            seed,
            schedule,
            parent: Some(self.trace.id()),
            ..TraceHeader::default()
        };

        ReplayResult {
            branch: Trace {
                header,
                events: self.branch,
            },
            divergences: self.divergences,
            holes: self.holes,
        }
    }

    /// Record what the agent actually did, into the branch trace.
    /// Append an effect to the branch.
    ///
    /// `outcome_digest` is `Some` when the outcome came from the recording,
    /// and passing it through is **required, not merely faster**.
    ///
    /// The reason is worth stating, because a property test found it and it
    /// would have been very hard to find any other way. Round-tripping a
    /// payload through `parse -> Value -> serialise` is not guaranteed to
    /// reproduce the original bytes: floating-point values can shift by one
    /// unit in the last place, so an outcome carrying a jitter value of
    /// `0.09466872426617445` comes back as `...44`. Different bytes mean a
    /// different digest, which means a replay that is faithful in every
    /// observable way still fails to compare equal to its own recording.
    ///
    /// The rule that falls out of this is general and worth holding onto:
    /// **a digest must be taken from bytes that were preserved, never from
    /// bytes that were regenerated.**
    fn emit(
        &mut self,
        request: &EffectRequest,
        outcome: &Outcome,
        outcome_digest: Option<Digest>,
        fault: Option<Fault>,
        shadow_digest: Option<Digest>,
    ) {
        let identity = request.identity(&self.canon);

        // The request is built fresh by the agent on every run, so there are
        // no original bytes to preserve; serialising is the only option and
        // is safe, because the agent constructed the value rather than
        // parsing it.
        let request_digest = self
            .cas
            .put(&serde_json::to_vec(request).expect("serialises"))
            .expect("cas write");

        let outcome_digest = match outcome_digest {
            Some(d) => d,
            None => self
                .cas
                .put(&serde_json::to_vec(outcome).expect("serialises"))
                .expect("cas write"),
        };

        self.branch.push(Event {
            seq: self.branch.len() as u64,
            kind: request.kind,
            name: request.name.clone(),
            identity,
            request: request_digest,
            outcome: outcome_digest,
            fault,
            shadow: shadow_digest,
            logical_time: self.logical_time,
        });
        self.logical_time += 1;
    }

    /// Compare the incoming effect against the recorded one at the cursor.
    fn check(
        &self,
        recorded: &Event,
        request: &EffectRequest,
        identity: &Digest,
    ) -> Option<DivergenceKind> {
        if recorded.kind != request.kind {
            return Some(DivergenceKind::Kind {
                expected: recorded.kind,
                found: request.kind,
            });
        }
        if recorded.name != request.name {
            return Some(DivergenceKind::Name {
                expected: recorded.name.clone(),
                found: request.name.clone(),
            });
        }
        if &recorded.identity != identity {
            // Pull the recorded request back out so we can say *where* they
            // differ, not merely that they do.
            let first = self
                .cas
                .get(&recorded.request)
                .ok()
                .flatten()
                .and_then(|b| serde_json::from_slice::<EffectRequest>(&b).ok())
                .and_then(|old| {
                    first_difference(
                        &self.canon.canonicalize(&old.body),
                        &self.canon.canonicalize(&request.body),
                        "",
                    )
                });
            return Some(DivergenceKind::Identity {
                expected: recorded.identity.clone(),
                found: identity.clone(),
                first_difference: first,
            });
        }
        None
    }

    fn recorded_outcome(&self, event: &Event) -> Outcome {
        self.cas
            .get(&event.outcome)
            .ok()
            .flatten()
            .and_then(|b| serde_json::from_slice::<Outcome>(&b).ok())
            .unwrap_or_else(|| Outcome::err("samsara_missing_payload", "outcome not in store"))
    }
}

impl<C: Cas> Replayer<'_, C> {
    /// Resolve an effect once we are off-script.
    ///
    /// Returns the outcome and, when it came from the recording, the digest
    /// of its original bytes.
    fn resolve_free(
        &mut self,
        request: &EffectRequest,
        identity: &Digest,
    ) -> (Outcome, Option<Digest>) {
        match request.kind {
            // Clock and randomness are generated, never recalled. Serving a
            // recorded timestamp here would freeze time for the rest of the
            // branch, and an agent whose clock has stopped computes a zero
            // backoff and spins forever.
            EffectKind::Clock => {
                self.free_clock_ms += FREE_CLOCK_TICK_MS;
                (Outcome::ok(Value::from(self.free_clock_ms)), None)
            }
            EffectKind::Random => (Outcome::ok(Value::from(self.free_rng.gen::<f64>())), None),

            // Everything else comes from the oracle. This is where a retry
            // gets served: it carries the same identity as the call it
            // retries, so the recording already knows the answer.
            EffectKind::Model | EffectKind::Tool => match self.oracle.answer(identity) {
                Some((digest, outcome)) => (outcome, Some(digest)),
                None => {
                    self.holes.push(Hole {
                        seq: self.branch.len() as u64,
                        kind: request.kind,
                        name: request.name.clone(),
                        identity: identity.clone(),
                    });
                    let outcome = Outcome::err(
                        "samsara_unresolved",
                        format!(
                            "no recorded response for `{}`; re-record with a live backend to cover this path",
                            request.name
                        ),
                    );
                    (outcome, None)
                }
            },
        }
    }
}

impl<C: Cas> Effects for Replayer<'_, C> {
    fn perform(&mut self, request: EffectRequest) -> Outcome {
        let identity = request.identity(&self.canon);

        // Faults are keyed by position in *this* run, not in the recording.
        //
        // Keying them to the recording seems natural and is wrong: the first
        // injected fault knocks the run off-script, after which "recorded
        // effect #7" no longer corresponds to anything the agent is doing,
        // and every later fault in the schedule silently fails to fire. That
        // makes multi-fault schedules meaningless and leaves the shrinker
        // nothing to shrink. Branch position stays well defined after the
        // fork, and expresses what a chaos harness actually wants to say:
        // *fail the fourth call this agent makes*.
        let position = self.branch.len() as u64;

        // --- 1. What would have happened, absent any fault? ---
        let (observed, observed_digest) = if self.free_running {
            self.resolve_free(&request, &identity)
        } else {
            match self.trace.events.get(self.cursor).cloned() {
                Some(recorded) => match self.check(&recorded, &request, &identity) {
                    Some(kind) => {
                        self.divergences.push(Divergence {
                            at_seq: position,
                            kind,
                        });
                        self.free_running = true;
                        self.resolve_free(&request, &identity)
                    }
                    None => {
                        self.cursor += 1;
                        (
                            self.recorded_outcome(&recorded),
                            Some(recorded.outcome.clone()),
                        )
                    }
                },
                None => {
                    self.divergences.push(Divergence {
                        at_seq: position,
                        kind: DivergenceKind::PastEnd {
                            recorded: self.trace.events.len() as u64,
                        },
                    });
                    self.free_running = true;
                    self.resolve_free(&request, &identity)
                }
            }
        };

        // --- 2. Is a fault scheduled here? ---
        let scheduled = match &self.mode {
            Mode::Counterfactual { schedule } => schedule.at(position).cloned(),
            Mode::Strict => None,
        };

        match scheduled {
            Some(fault) if fault.applies_to(request.kind) => {
                // From here the run is no longer the recorded run, and we
                // stop expecting it to be.
                self.free_running = true;
                let faulted = fault.apply(&observed);

                // Preserve what really happened. The agent will never see it;
                // the invariant checker will, and it is the difference
                // between "these calls may both have taken effect" and "they
                // did".
                let shadow = observed_digest.clone().or_else(|| {
                    Some(
                        self.cas
                            .put(&serde_json::to_vec(&observed).expect("serialises"))
                            .expect("cas write"),
                    )
                });

                self.emit(&request, &faulted, None, Some(fault), shadow);
                faulted
            }
            _ => {
                self.emit(&request, &observed, observed_digest, None, None);
                observed
            }
        }
    }
}

/// What a replay produced.
#[derive(Clone, Debug)]
pub struct ReplayResult {
    /// The run as it actually happened this time. Equal to the original in a
    /// faithful strict replay; the counterfactual timeline otherwise.
    pub branch: Trace,
    pub divergences: Vec<Divergence>,
    pub holes: Vec<Hole>,
}

impl ReplayResult {
    /// True when the agent followed the recording exactly.
    pub fn is_faithful(&self) -> bool {
        self.divergences.is_empty() && self.holes.is_empty()
    }

    /// The first divergence, which is the only one worth reading — everything
    /// after it is downstream of it.
    pub fn first_divergence(&self) -> Option<&Divergence> {
        self.divergences.first()
    }
}
