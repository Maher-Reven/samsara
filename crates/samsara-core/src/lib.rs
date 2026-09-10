//! # Samsara
//!
//! Deterministic replay and fault injection for LLM agent runs.

#![forbid(unsafe_code)]
#![warn(missing_debug_implementations)]

pub mod canon;
pub mod cas;
pub mod event;
pub mod fault;
pub mod hash;
pub mod invariant;
pub mod replay;
pub mod trace;

/// The types you need to use Samsara, in one import.
pub mod prelude {
    pub use crate::canon::Canonicalizer;
    pub use crate::cas::{Cas, FsCas, MemCas};
    pub use crate::event::{EffectKind, EffectRequest, Event, Outcome};
    pub use crate::fault::{Fault, FaultPoint, FaultSchedule};
    pub use crate::hash::Digest;
    pub use crate::invariant::{
        check_all, Invariant, NoDuplicateEffects, TerminatesWithin, TokenBudget, Violation,
    };
    pub use crate::replay::{
        Backend, Divergence, DivergenceKind, Effects, Mode, Recorder, ReplayResult, Replayer,
    };
    pub use crate::trace::{Trace, TraceHeader};
}
