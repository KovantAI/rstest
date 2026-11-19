//! Directory transport: a local path, a network mount, or a directory
//! another tool materializes.

use crate::cache;
use anyhow::{Context, Result};
use std::path::PathBuf;

use super::Transport;

/// Filesystem/network-dir backend: `<root>/base.json` + `<root>/segments/seg-<id>.json`.
/// Works over a local path, an NFS/EFS mount, or a directory another tool
/// (`aws s3 sync`, `download-artifact`) materializes.
pub struct DirTransport {
    root: PathBuf,
}

impl DirTransport {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    fn segments_dir(&self) -> PathBuf {
        self.root.join("segments")
    }
    fn segment_path(&self, id: &str) -> PathBuf {
        self.segments_dir().join(format!("seg-{id}.json"))
    }
    fn base_path(&self) -> PathBuf {
        self.root.join("base.json")
    }
}

impl Transport for DirTransport {
    fn list_segment_ids(&self) -> Result<Vec<String>> {
        let dir = self.segments_dir();
        let rd = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("listing {}", dir.display())),
        };
        let mut ids = Vec::new();
        for entry in rd.flatten() {
            if let Some(name) = entry.file_name().to_str() {
                if let Some(id) = name
                    .strip_prefix("seg-")
                    .and_then(|s| s.strip_suffix(".json"))
                {
                    ids.push(id.to_string());
                }
            }
        }
        Ok(ids)
    }
    fn read_segment(&self, id: &str) -> Result<Option<Vec<u8>>> {
        let p = self.segment_path(id);
        match std::fs::read(&p) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading segment {}", p.display())),
        }
    }
    fn read_base(&self) -> Result<Option<Vec<u8>>> {
        match std::fs::read(self.base_path()) {
            Ok(b) => Ok(Some(b)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).context("reading base.json"),
        }
    }
    fn write_segment(&self, id: &str, bytes: &[u8]) -> Result<()> {
        let p = self.segment_path(id);
        cache::write_atomic(&p, bytes).with_context(|| format!("writing segment {}", p.display()))
    }
    fn write_base(&self, bytes: &[u8]) -> Result<()> {
        cache::write_atomic(&self.base_path(), bytes).context("writing base.json")
    }
    fn delete_segment(&self, id: &str) -> Result<()> {
        match std::fs::remove_file(self.segment_path(id)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).context("deleting segment"),
        }
    }
}
