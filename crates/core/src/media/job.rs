//! Async media-job store (dev-plan/40, Tier 2).
//!
//! Video generation is long-running (Veo: 30–120s), so the
//! `TextToVideo` / `ImageToVideo` tools submit a job and return a
//! `job_id`; `MediaJobStatus` polls it. This module persists each job's
//! state to `.thclaws/media-jobs.jsonl` (append-only; latest line per id
//! wins) so a poll survives an engine restart — the provider-side
//! operation ref outlives the process, we just re-attach.
//!
//! The store is file-backed on every call (low volume — a handful of
//! jobs), serialised by a process-global mutex. No in-memory cache to
//! diverge from disk; resume is just "read the log".

use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

fn log_path() -> PathBuf {
    std::path::Path::new(".thclaws").join("media-jobs.jsonl")
}

/// Terminal + in-flight states. Stringly-typed for forward-compatible
/// JSONL (an old binary reading a newer state value just sees the
/// string).
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_DONE: &str = "done";
pub const STATUS_FAILED: &str = "failed";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaJob {
    pub id: String,
    /// "text2video" | "image2video".
    pub kind: String,
    pub provider: String,
    pub model: String,
    /// Provider-side operation ref to poll (e.g. a Veo operation name).
    pub op: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub asset_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub duration_seconds: u32,
    pub created_at: String,
}

impl MediaJob {
    /// Deterministic short id from the provider op (op names are unique
    /// per submission) — no uuid dep needed.
    pub fn new_id(op: &str) -> String {
        let d = Sha256::digest(op.as_bytes());
        format!(
            "vid-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            d[0], d[1], d[2], d[3], d[4], d[5]
        )
    }
    pub fn is_terminal(&self) -> bool {
        self.status == STATUS_DONE || self.status == STATUS_FAILED
    }
}

fn append(job: &MediaJob) -> Result<()> {
    use std::io::Write;
    let path = log_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| Error::Tool(format!("mkdir .thclaws: {e}")))?;
    }
    let line =
        serde_json::to_string(job).map_err(|e| Error::Tool(format!("serialize media job: {e}")))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .map_err(|e| Error::Tool(format!("open {}: {e}", path.display())))?;
    writeln!(f, "{line}").map_err(|e| Error::Tool(format!("write media job: {e}")))?;
    Ok(())
}

/// Fold the append-log into the latest state per id.
fn load_at(path: &PathBuf) -> Result<Vec<MediaJob>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Tool(format!("read {}: {e}", path.display()))),
    };
    // Last line per id wins; preserve first-seen order for listing.
    let mut order: Vec<String> = Vec::new();
    let mut latest: std::collections::HashMap<String, MediaJob> = std::collections::HashMap::new();
    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Ok(job) = serde_json::from_str::<MediaJob>(line) {
            if !latest.contains_key(&job.id) {
                order.push(job.id.clone());
            }
            latest.insert(job.id.clone(), job);
        }
    }
    Ok(order
        .into_iter()
        .filter_map(|id| latest.remove(&id))
        .collect())
}

fn load_all() -> Result<Vec<MediaJob>> {
    load_at(&log_path())
}

/// Compact the log file at `path` to one entry per job id.
/// Returns the number of duplicate lines removed.
/// Caller must NOT hold LOCK.
fn prune_at(path: &PathBuf) -> Result<usize> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::Tool(format!("read {}: {e}", path.display()))),
    };
    let original = raw.lines().filter(|l| !l.trim().is_empty()).count();
    let jobs = load_at(path)?;
    if jobs.len() == original {
        return Ok(0);
    }
    let mut content = String::new();
    for job in &jobs {
        let line = serde_json::to_string(job)
            .map_err(|e| Error::Tool(format!("serialize media job: {e}")))?;
        content.push_str(&line);
        content.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, &content)
        .map_err(|e| Error::Tool(format!("write {}: {e}", tmp.display())))?;
    if std::fs::rename(&tmp, path).is_err() {
        // cross-device fallback
        std::fs::copy(&tmp, path)
            .map_err(|e| Error::Tool(format!("copy to {}: {e}", path.display())))?;
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(original - jobs.len())
}

fn prune_unlocked() -> Result<usize> {
    prune_at(&log_path())
}

/// Compact the log to one entry per job id.
/// Returns the number of lines removed.
pub fn prune() -> Result<usize> {
    let _g = LOCK.lock().unwrap();
    prune_unlocked()
}

/// Persist a newly-submitted job.
pub fn create(job: &MediaJob) -> Result<()> {
    let _g = LOCK.lock().unwrap();
    append(job)
}

/// Look up the latest state of a job by id.
pub fn get(id: &str) -> Result<Option<MediaJob>> {
    let _g = LOCK.lock().unwrap();
    Ok(load_all()?.into_iter().find(|j| j.id == id))
}

/// Append a new state snapshot for an existing job.
pub fn update(job: &MediaJob) -> Result<()> {
    let _g = LOCK.lock().unwrap();
    append(job)?;
    if job.is_terminal() {
        let _ = prune_unlocked();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_deterministic_per_op() {
        let a = MediaJob::new_id("operations/abc123");
        let b = MediaJob::new_id("operations/abc123");
        let c = MediaJob::new_id("operations/different");
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert!(a.starts_with("vid-"));
    }

    #[test]
    fn prune_compacts_duplicate_entries() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("media-jobs.jsonl");
        let job = MediaJob {
            id: "vid-aabbcc".into(),
            kind: "text2video".into(),
            provider: "veo".into(),
            model: "veo-3.1-fast-generate-preview".into(),
            op: "operations/test".into(),
            status: STATUS_RUNNING.into(),
            asset_path: None,
            error: None,
            duration_seconds: 8,
            created_at: "2026-06-24".into(),
        };
        // write 3 entries for the same id
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write as _;
        for _ in 0..3 {
            writeln!(f, "{}", serde_json::to_string(&job).unwrap()).unwrap();
        }
        drop(f);

        let removed = super::prune_at(&path).unwrap();
        assert_eq!(removed, 2);
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.lines().filter(|l| !l.trim().is_empty()).count(), 1);
    }

    #[test]
    fn terminal_detection() {
        let mut j = MediaJob {
            id: "x".into(),
            kind: "text2video".into(),
            provider: "veo".into(),
            model: "veo-3.1-fast-generate-preview".into(),
            op: "operations/x".into(),
            status: STATUS_RUNNING.into(),
            asset_path: None,
            error: None,
            duration_seconds: 8,
            created_at: "2026-06-14".into(),
        };
        assert!(!j.is_terminal());
        j.status = STATUS_DONE.into();
        assert!(j.is_terminal());
        j.status = STATUS_FAILED.into();
        assert!(j.is_terminal());
    }
}
