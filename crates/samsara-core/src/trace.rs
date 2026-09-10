//! The trace: an append-only log of effects, plus the header that makes it
//! reproducible.
//!
//! Serialised as JSONL — header on line one, one event per line after. This
//! is a deliberate choice over a binary format: traces land in pull requests
//! as regression fixtures, and a fixture you cannot read in a diff is a
//! fixture nobody trusts. Payloads live in the CAS, so the lines stay short.

use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};

use crate::canon::Canonicalizer;
use crate::event::Event;
use crate::fault::FaultSchedule;
use crate::hash::Digest;

/// Format version. Bumped on any breaking change to the line schema; the
/// loader refuses versions it does not understand rather than guessing.
pub const FORMAT_VERSION: u32 = 1;

/// Everything needed to interpret the events that follow.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TraceHeader {
    pub format_version: u32,
    /// Samsara version that produced the trace.
    pub engine: String,
    /// Free-form label, usually the command that was recorded.
    #[serde(default)]
    pub label: String,
    /// Unix millis when recording began. Informational only — never used for
    /// ordering, which is always by `logical_time`.
    #[serde(default)]
    pub recorded_at_ms: u64,
    /// The identity policy in force. Stored so replay cannot silently apply
    /// different matching rules than recording did.
    pub canonicalizer: Canonicalizer,
    /// If this trace is a counterfactual branch, the seed that generated its
    /// schedule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seed: Option<u64>,
    /// The faults applied, if this is a branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<FaultSchedule>,
    /// The trace this one was forked from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<Digest>,
}

impl Default for TraceHeader {
    fn default() -> Self {
        TraceHeader {
            format_version: FORMAT_VERSION,
            engine: format!("samsara/{}", env!("CARGO_PKG_VERSION")),
            label: String::new(),
            recorded_at_ms: 0,
            canonicalizer: Canonicalizer::default(),
            seed: None,
            schedule: None,
            parent: None,
        }
    }
}

/// A recorded run.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Trace {
    pub header: TraceHeader,
    pub events: Vec<Event>,
}

/// Things that can go wrong reading a trace.
#[derive(Debug, thiserror::Error)]
pub enum TraceError {
    #[error("io: {0}")]
    Io(#[from] io::Error),
    #[error("malformed trace at line {line}: {source}")]
    Parse {
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("empty trace: expected a header on line 1")]
    Empty,
    #[error("unsupported trace format version {found} (this build reads {expected})")]
    Version { found: u32, expected: u32 },
    #[error("event sequence numbers are not contiguous from 0: expected {expected}, found {found} at line {line}")]
    Sequence {
        expected: u64,
        found: u64,
        line: usize,
    },
}

impl Trace {
    /// A new, empty trace.
    pub fn new(header: TraceHeader) -> Self {
        Trace {
            header,
            events: Vec::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Positions eligible for fault injection: everything except clock and
    /// random draws, which are replayed verbatim by definition.
    pub fn faultable(&self) -> Vec<u64> {
        self.events
            .iter()
            .filter(|e| e.kind.is_effectful() || e.kind == crate::event::EffectKind::Model)
            .map(|e| e.seq)
            .collect()
    }

    /// Identity of the trace itself: the digest of its serialised events.
    /// Two runs that performed the same effects with the same outcomes have
    /// the same id, regardless of when they ran.
    pub fn id(&self) -> Digest {
        let mut buf = Vec::new();
        for event in &self.events {
            buf.extend_from_slice(&serde_json::to_vec(event).expect("Event serialises"));
            buf.push(b'\n');
        }
        Digest::of(&buf)
    }

    /// Write as JSONL.
    pub fn write<W: Write>(&self, mut w: W) -> Result<(), TraceError> {
        serde_json::to_writer(&mut w, &self.header)
            .map_err(|e| TraceError::Parse { line: 1, source: e })?;
        w.write_all(b"\n")?;
        for event in &self.events {
            serde_json::to_writer(&mut w, event).map_err(|e| TraceError::Parse {
                line: event.seq as usize + 2,
                source: e,
            })?;
            w.write_all(b"\n")?;
        }
        w.flush()?;
        Ok(())
    }

    /// Read from JSONL.
    ///
    /// Validates the version and that sequence numbers are contiguous from
    /// zero. That second check matters: replay indexes events by position, so
    /// a trace with a gap would silently match the wrong effect. Better to
    /// refuse it.
    pub fn read<R: BufRead>(r: R) -> Result<Self, TraceError> {
        let mut lines = r.lines();

        let header_line = lines.next().ok_or(TraceError::Empty)??;
        let header: TraceHeader = serde_json::from_str(&header_line)
            .map_err(|e| TraceError::Parse { line: 1, source: e })?;

        if header.format_version != FORMAT_VERSION {
            return Err(TraceError::Version {
                found: header.format_version,
                expected: FORMAT_VERSION,
            });
        }

        let mut events = Vec::new();
        for (i, line) in lines.enumerate() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let event: Event = serde_json::from_str(&line).map_err(|e| TraceError::Parse {
                line: i + 2,
                source: e,
            })?;
            let expected = events.len() as u64;
            if event.seq != expected {
                return Err(TraceError::Sequence {
                    expected,
                    found: event.seq,
                    line: i + 2,
                });
            }
            events.push(event);
        }

        Ok(Trace { header, events })
    }

    /// Render as a JSONL string.
    pub fn to_jsonl(&self) -> String {
        let mut buf = Vec::new();
        self.write(&mut buf).expect("writing to a Vec cannot fail");
        String::from_utf8(buf).expect("serde_json emits UTF-8")
    }
}

impl std::str::FromStr for Trace {
    type Err = TraceError;

    /// Parse a trace from a string of JSONL.
    fn from_str(s: &str) -> Result<Self, TraceError> {
        Trace::read(io::Cursor::new(s.as_bytes()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::EffectKind;
    use crate::hash::Digest;
    use std::str::FromStr;

    fn event(seq: u64) -> Event {
        Event {
            seq,
            kind: EffectKind::Tool,
            name: format!("tool_{seq}"),
            identity: Digest::of(format!("id{seq}").as_bytes()),
            request: Digest::of(b"req"),
            outcome: Digest::of(b"out"),
            fault: None,
            shadow: None,
            logical_time: seq,
            batch: None,
        }
    }

    fn trace_of(n: u64) -> Trace {
        let mut t = Trace::new(TraceHeader::default());
        t.events = (0..n).map(event).collect();
        t
    }

    #[test]
    fn roundtrips_through_jsonl() {
        let t = trace_of(5);
        let back = Trace::from_str(&t.to_jsonl()).unwrap();
        assert_eq!(t, back);
    }

    #[test]
    fn header_occupies_exactly_the_first_line() {
        let t = trace_of(3);
        let text = t.to_jsonl();
        assert_eq!(text.lines().count(), 4, "1 header + 3 events");
        let first: serde_json::Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
        assert!(first.get("format_version").is_some());
    }

    #[test]
    fn identity_ignores_the_header() {
        let a = trace_of(4);
        let mut b = trace_of(4);
        b.header.label = "a completely different label".into();
        b.header.recorded_at_ms = 1_700_000_000_000;
        assert_eq!(
            a.id(),
            b.id(),
            "trace identity is its effects, not its metadata"
        );
    }

    #[test]
    fn refuses_a_future_format_version() {
        let mut t = trace_of(1);
        t.header.format_version = FORMAT_VERSION + 1;
        let err = Trace::from_str(&t.to_jsonl()).unwrap_err();
        assert!(matches!(err, TraceError::Version { .. }));
    }

    #[test]
    fn refuses_a_gap_in_the_sequence() {
        let mut t = trace_of(3);
        t.events[1].seq = 7; // a gap replay would silently mis-index
        let err = Trace::from_str(&t.to_jsonl()).unwrap_err();
        assert!(matches!(
            err,
            TraceError::Sequence {
                expected: 1,
                found: 7,
                ..
            }
        ));
    }

    #[test]
    fn refuses_an_empty_file() {
        assert!(matches!(Trace::from_str(""), Err(TraceError::Empty)));
    }

    #[test]
    fn tolerates_trailing_blank_lines() {
        let text = format!("{}\n\n", trace_of(2).to_jsonl());
        assert_eq!(Trace::from_str(&text).unwrap().len(), 2);
    }
}
