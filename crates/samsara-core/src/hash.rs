//! Content addressing.
//!
//! Every payload Samsara stores — a model request, a tool result, a fault
//! schedule — is named by the BLAKE3 hash of its bytes. Two runs that produced
//! the same bytes therefore share storage, which matters more than it sounds:
//! a counterfactual branch usually differs from its parent in one tool result
//! out of hundreds, and we would rather not pay for the other ninety-nine.

use serde::{Deserialize, Serialize};
use std::fmt;

/// A BLAKE3 digest, rendered as lowercase hex.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Digest(String);

impl Digest {
    /// Hash a byte slice.
    pub fn of(bytes: &[u8]) -> Self {
        Digest(blake3::hash(bytes).to_hex().to_string())
    }

    /// The full hex digest.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// First 12 hex chars — enough to disambiguate by eye in a terminal, and
    /// what we print in divergence reports.
    pub fn short(&self) -> &str {
        &self.0[..12]
    }

    /// Reconstruct from hex. Does not verify the digest corresponds to any
    /// content; that is the CAS's job on read.
    pub fn from_hex(s: impl Into<String>) -> Self {
        Digest(s.into())
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Digest({})", self.short())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_is_stable_and_distinguishing() {
        assert_eq!(Digest::of(b"hello"), Digest::of(b"hello"));
        assert_ne!(Digest::of(b"hello"), Digest::of(b"hellp"));
        assert_eq!(Digest::of(b"hello").short().len(), 12);
    }
}
