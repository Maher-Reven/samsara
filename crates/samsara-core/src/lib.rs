//! # Samsara
//!
//! Deterministic replay and fault injection for LLM agent runs.
//!
//! An agent run is deterministic apart from the effects it performs. Record
//! every effect and you can re-run the agent offline, feeding it the recorded
//! answers. Do that, and three things become possible that were not before:
//!
//! 1. **Replay.** A production failure runs again on your laptop, in CI, next
//!    year, with no API key and no cost.
//! 2. **Counterfactuals.** Replay faithfully to step *n*, then inject a fault
//!    and let the agent run free. Does it recover, or corrupt its state?
//! 3. **Search.** Because a fault schedule is generated from a `u64`, "does
//!    this agent survive adversity" becomes a search over seeds — and a
//!    failure is reported as a single integer.
//!
//! ```no_run
//! use samsara_core::prelude::*;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! # let trace: Trace = "".parse()?;
//! # let mut cas = MemCas::new();
//! // Reproduce a failure from its seed.
//! let schedule = FaultSchedule::generate(91238, &trace.faultable(), 4);
//! let mut replayer = Replayer::new(&trace, &mut cas, Mode::Counterfactual { schedule })?;
//! // ... drive the agent against `replayer` ...
//! let result = replayer.finish("repro");
//!
//! // Did it break an invariant?
//! let violations = NoDuplicateEffects::all().check(&result.branch, &cas);
//! # Ok(())
//! # }
//! ```

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod canon;
pub mod cas;
pub mod certificate;
pub mod event;
pub mod explore;
pub mod fault;
pub mod hash;
pub mod invariant;
pub mod replay;
pub mod shrink;
pub mod trace;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;

/// The types you need to use Samsara, in one import.
pub mod prelude {
    pub use crate::canon::Canonicalizer;
    pub use crate::cas::{Cas, FsCas, MemCas};
    pub use crate::certificate::{Certificate, Verdict};
    pub use crate::event::{EffectKind, EffectRequest, Event, Outcome};
    pub use crate::explore::{
        evaluate, interleavings, minimise, search, search_order_dependence, sweep, Case, Coverage,
        Finding, Interleavings, OrderDependence,
    };
    pub use crate::fault::{Fault, FaultPoint, FaultSchedule};
    pub use crate::hash::Digest;
    pub use crate::invariant::{
        check_all, Invariant, NoDuplicateEffects, TerminatesWithin, TokenBudget, Violation,
    };
    pub use crate::replay::{
        Backend, Divergence, DivergenceKind, Effects, Mode, Recorder, ReplayResult, Replayer,
    };
    pub use crate::shrink::{shrink, Shrunk};
    pub use crate::trace::{Trace, TraceHeader};
}
