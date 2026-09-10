//! Content-addressed storage for effect payloads.
//!
//! The trace itself stays small and human-diffable: a line per effect, each
//! naming its payloads by digest. The payloads — which are mostly large,
//! mostly repeated across branches — live here.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::hash::Digest;

/// A content-addressed blob store.
pub trait Cas {
    /// Store bytes, returning their digest. Storing the same bytes twice is a
    /// no-op that returns the same digest.
    fn put(&mut self, bytes: &[u8]) -> io::Result<Digest>;

    /// Retrieve bytes by digest.
    fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>>;

    /// Whether the store holds this digest.
    fn has(&self, digest: &Digest) -> bool {
        matches!(self.get(digest), Ok(Some(_)))
    }
}

/// In-memory store. Used by tests and by the WASM build, where there is no
/// filesystem and the whole trace arrives as one bundle.
#[derive(Default, Debug, Clone)]
pub struct MemCas {
    objects: HashMap<Digest, Vec<u8>>,
}

impl MemCas {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of distinct objects held.
    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    /// Total bytes held, ignoring map overhead.
    pub fn bytes(&self) -> usize {
        self.objects.values().map(|v| v.len()).sum()
    }

    /// Iterate every stored object. Used when bundling a trace for the viewer.
    pub fn iter(&self) -> impl Iterator<Item = (&Digest, &Vec<u8>)> {
        self.objects.iter()
    }
}

impl Cas for MemCas {
    fn put(&mut self, bytes: &[u8]) -> io::Result<Digest> {
        let digest = Digest::of(bytes);
        self.objects
            .entry(digest.clone())
            .or_insert_with(|| bytes.to_vec());
        Ok(digest)
    }

    fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>> {
        Ok(self.objects.get(digest).cloned())
    }

    fn has(&self, digest: &Digest) -> bool {
        self.objects.contains_key(digest)
    }
}

/// Filesystem store, sharded two hex chars deep so a long-lived project does
/// not end up with a hundred thousand entries in one directory.
#[derive(Debug, Clone)]
pub struct FsCas {
    root: PathBuf,
}

impl FsCas {
    /// Open (creating if absent) a store rooted at `root`.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        Ok(FsCas { root })
    }

    fn path_for(&self, digest: &Digest) -> PathBuf {
        let hex = digest.as_str();
        self.root.join(&hex[..2]).join(&hex[2..])
    }
}

impl Cas for FsCas {
    fn put(&mut self, bytes: &[u8]) -> io::Result<Digest> {
        let digest = Digest::of(bytes);
        let path = self.path_for(&digest);
        if path.exists() {
            // Content-addressed: identical digest means identical bytes, so
            // rewriting would only risk tearing a file another reader holds.
            return Ok(digest);
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        // Write-then-rename so a reader never observes a partial object.
        let tmp = path.with_extension("tmp");
        fs::write(&tmp, bytes)?;
        fs::rename(&tmp, &path)?;
        Ok(digest)
    }

    fn get(&self, digest: &Digest) -> io::Result<Option<Vec<u8>>> {
        match fs::read(self.path_for(digest)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn has(&self, digest: &Digest) -> bool {
        self.path_for(digest).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip<C: Cas>(mut cas: C) {
        let d1 = cas.put(b"alpha").unwrap();
        let d2 = cas.put(b"beta").unwrap();
        let d1_again = cas.put(b"alpha").unwrap();

        assert_eq!(d1, d1_again, "identical bytes deduplicate");
        assert_ne!(d1, d2);
        assert_eq!(cas.get(&d1).unwrap().unwrap(), b"alpha");
        assert_eq!(cas.get(&d2).unwrap().unwrap(), b"beta");
        assert!(cas.has(&d1));
        assert!(cas.get(&Digest::of(b"never stored")).unwrap().is_none());
    }

    #[test]
    fn mem_cas_roundtrips() {
        roundtrip(MemCas::new());
    }

    #[test]
    fn fs_cas_roundtrips() {
        let dir = std::env::temp_dir().join(format!("samsara-cas-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        roundtrip(FsCas::open(&dir).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn mem_cas_deduplicates_storage() {
        let mut cas = MemCas::new();
        let big = vec![7u8; 4096];
        for _ in 0..10 {
            cas.put(&big).unwrap();
        }
        assert_eq!(cas.len(), 1);
        assert_eq!(cas.bytes(), 4096, "ten identical payloads cost one");
    }
}
