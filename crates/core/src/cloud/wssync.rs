//! Workspace sync (dev-plan/51): tar/untar the working directory for the
//! `/cloud push|pull` round-trip between the desktop app and a hosted cloud
//! workspace.
//!
//! Reuses [`crate::cloud::pack::is_strippable`] for the exclude set (drops
//! `.thclaws/sessions/`, usage, team, …) and adds the sync-specific pieces:
//!   - a 250 MB payload cap (`MAX_SYNC_BYTES`),
//!   - `--delete` mirroring that moves removed files to `.sync-trash/<ts>/`
//!     (recoverable, not a hard delete),
//!   - traversal-safe extraction (rejects `..` / absolute, skips symlinks),
//!   - the UUID binding file `.thclaws/cloud-sync.json` that ties a local folder
//!     to exactly one hosted workspace.
//!
//! v1 is whole-tarball (`.tar.gz`, matching `pack.rs`); the incremental
//! manifest-diff path layers on top in P2.

use crate::cloud::pack::is_strippable;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Hard cap on a single sync payload (uncompressed sum of synced files).
pub const MAX_SYNC_BYTES: u64 = 250 * 1024 * 1024;

const BINDING_REL: &str = ".thclaws/cloud-sync.json";
const SETTINGS_REL: &str = ".thclaws/settings.json";
const TRASH_PREFIX: &str = ".sync-trash";

/// Records which hosted workspace a folder is paired with. Lives at
/// `.thclaws/cloud-sync.json` on both ends (dev-plan/51 decision #5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Binding {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slug: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cloud_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_push: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_pull: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SyncStat {
    pub file_count: usize,
    pub bytes: u64,
}

#[derive(Debug, Clone, Default)]
pub struct UntarResult {
    pub written: usize,
    pub deleted: usize,
    pub trash_dir: Option<PathBuf>,
}

fn norm(rel: &Path) -> String {
    rel.to_string_lossy().replace('\\', "/")
}

/// Inside the sync exclude set? Runtime artifacts (via `is_strippable`) plus the
/// `.sync-trash/` tree itself (never sync the trash).
fn excluded(rel: &Path) -> bool {
    let s = norm(rel);
    s == TRASH_PREFIX || s.starts_with(&format!("{TRASH_PREFIX}/")) || is_strippable(rel)
}

/// Collect files relative to `root`. `keep` decides inclusion; symlinks are
/// always skipped (never followed — traversal safety).
fn walk(root: &Path, keep: &dyn Fn(&Path) -> bool) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    walk_inner(root, root, keep, &mut out)?;
    out.sort();
    Ok(out)
}

fn walk_inner(
    root: &Path,
    dir: &Path,
    keep: &dyn Fn(&Path) -> bool,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("read_dir {}: {}", dir.display(), e)),
    };
    for ent in rd {
        let ent = ent.map_err(|e| format!("dir entry: {}", e))?;
        let path = ent.path();
        let rel = match path.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        if !keep(&rel) {
            continue;
        }
        let ft = ent.file_type().map_err(|e| format!("file_type: {}", e))?;
        if ft.is_symlink() {
            continue; // never follow or sync symlinks
        } else if ft.is_dir() {
            walk_inner(root, &path, keep, out)?;
        } else if ft.is_file() {
            out.push(rel);
        }
    }
    Ok(())
}

/// Synced (non-excluded) files, relative to `root`.
fn walk_synced(root: &Path) -> Result<Vec<PathBuf>, String> {
    walk(root, &|rel| !excluded(rel))
}

pub fn stat_workspace(root: &Path) -> Result<SyncStat, String> {
    let files = walk_synced(root)?;
    let mut bytes = 0u64;
    for rel in &files {
        bytes += std::fs::metadata(root.join(rel))
            .map(|m| m.len())
            .unwrap_or(0);
    }
    Ok(SyncStat {
        file_count: files.len(),
        bytes,
    })
}

/// "Empty" for the binding guard: no synced files other than a bare
/// `.thclaws/settings.json` (a fresh workspace ships only settings).
pub fn is_empty(root: &Path) -> Result<bool, String> {
    Ok(walk_synced(root)?.iter().all(|r| norm(r) == SETTINGS_REL))
}

pub fn read_binding(root: &Path) -> Binding {
    std::fs::read(root.join(BINDING_REL))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn write_binding(root: &Path, b: &Binding) -> Result<(), String> {
    let p = root.join(BINDING_REL);
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    }
    let data = serde_json::to_vec_pretty(b).map_err(|e| format!("serialize binding: {}", e))?;
    std::fs::write(&p, data).map_err(|e| format!("write binding: {}", e))
}

/// Tar+gzip the synced files under `root`. `include_runtime` bypasses the strip
/// set (still skips `.sync-trash/`). Enforces `MAX_SYNC_BYTES`.
pub fn tar_workspace(root: &Path, include_runtime: bool) -> Result<Vec<u8>, String> {
    let files = if include_runtime {
        walk(root, &|rel| {
            let s = norm(rel);
            s != TRASH_PREFIX && !s.starts_with(&format!("{TRASH_PREFIX}/"))
        })?
    } else {
        walk_synced(root)?
    };
    let total: u64 = files
        .iter()
        .map(|r| {
            std::fs::metadata(root.join(r))
                .map(|m| m.len())
                .unwrap_or(0)
        })
        .sum();
    if total > MAX_SYNC_BYTES {
        return Err(format!(
            "workspace is {} MB, over the {} MB sync cap",
            total / 1_048_576,
            MAX_SYNC_BYTES / 1_048_576
        ));
    }
    let enc = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(enc);
    for rel in &files {
        let abs = root.join(rel);
        let mut f =
            std::fs::File::open(&abs).map_err(|e| format!("open {}: {}", abs.display(), e))?;
        tar.append_file(rel, &mut f)
            .map_err(|e| format!("tar append {}: {}", rel.display(), e))?;
    }
    let enc = tar.into_inner().map_err(|e| format!("tar finish: {}", e))?;
    enc.finish().map_err(|e| format!("gz finish: {}", e))
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Canonicalize `root`, creating it if missing.
fn canonical_root(root: &Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root).map_err(|e| format!("mkdir {}: {}", root.display(), e))?;
    root.canonicalize()
        .map_err(|e| format!("canonicalize {}: {}", root.display(), e))
}

/// Extract a `.tar.gz` into the (canonical) `root`, overwriting in place.
/// Traversal-safe. Returns (files written, set of incoming relative paths).
fn extract_tarball(bytes: &[u8], root: &Path) -> Result<(usize, BTreeSet<PathBuf>), String> {
    let mut written = 0usize;
    let mut incoming: BTreeSet<PathBuf> = BTreeSet::new();
    let mut archive = tar::Archive::new(GzDecoder::new(bytes));
    for entry in archive
        .entries()
        .map_err(|e| format!("read archive: {}", e))?
    {
        let mut entry = entry.map_err(|e| format!("read entry: {}", e))?;
        let path = entry
            .path()
            .map_err(|e| format!("entry path: {}", e))?
            .into_owned();
        if is_unsafe_entry(&path) {
            return Err(format!("refused unsafe entry path: {}", path.display()));
        }
        if entry.header().entry_type().is_dir() {
            continue;
        }
        let out = root.join(&path);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
        }
        let mut f =
            std::fs::File::create(&out).map_err(|e| format!("create {}: {}", out.display(), e))?;
        std::io::copy(&mut entry, &mut f).map_err(|e| format!("write {}: {}", out.display(), e))?;
        incoming.insert(path);
        written += 1;
    }
    Ok((written, incoming))
}

/// Extract a full `.tar.gz` into `root`, overwriting in place. When `delete` is
/// set, synced files not present in the tarball are moved to `.sync-trash/<ts>/`
/// (recoverable mirror).
pub fn untar_workspace(bytes: &[u8], root: &Path, delete: bool) -> Result<UntarResult, String> {
    let root = canonical_root(root)?;
    let (written, incoming) = extract_tarball(bytes, &root)?;
    let trash = root.join(TRASH_PREFIX).join(unix_secs().to_string());
    let mut trash_used = false;
    let mut deleted = 0usize;
    if delete {
        for rel in walk_synced(&root)? {
            if !incoming.contains(&rel) {
                move_to_trash(&root, &trash, &rel, &mut trash_used)?;
                deleted += 1;
            }
        }
    }
    Ok(UntarResult {
        written,
        deleted,
        trash_dir: trash_used.then_some(trash),
    })
}

/// Reject archive entries that would escape the extraction root.
fn is_unsafe_entry(path: &Path) -> bool {
    path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir))
}

fn move_to_trash(root: &Path, trash: &Path, rel: &Path, used: &mut bool) -> Result<(), String> {
    let src = root.join(rel);
    if !src.exists() {
        return Ok(());
    }
    let dst = trash.join(rel);
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("mkdir trash {}: {}", parent.display(), e))?;
    }
    std::fs::rename(&src, &dst)
        .or_else(|_| {
            std::fs::copy(&src, &dst)
                .and_then(|_| std::fs::remove_file(&src))
                .map(|_| ())
        })
        .map_err(|e| format!("trash {}: {}", rel.display(), e))?;
    *used = true;
    Ok(())
}

// ---- P2: incremental manifest-diff ----

/// One file's identity in a sync manifest. `sha256` is the diff key (mtime is
/// unreliable across machines, so we hash content).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(data);
    let mut s = String::with_capacity(64);
    for b in digest {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Content manifest of the synced files under `root` (the diff input).
pub fn build_manifest(root: &Path) -> Result<Vec<FileEntry>, String> {
    let mut out = Vec::new();
    for rel in walk_synced(root)? {
        let data =
            std::fs::read(root.join(&rel)).map_err(|e| format!("read {}: {}", rel.display(), e))?;
        out.push(FileEntry {
            path: norm(&rel),
            size: data.len() as u64,
            sha256: sha256_hex(&data),
        });
    }
    Ok(out)
}

/// Compare a source manifest against a destination manifest. Returns
/// `(to_transfer, extraneous)`: files in `src` that are missing or
/// content-different in `dst` (must be sent src→dst), and files in `dst` not in
/// `src` (candidates for `--delete`). Pure — the unit-testable heart of P2.
pub fn diff(src: &[FileEntry], dst: &[FileEntry]) -> (Vec<String>, Vec<String>) {
    use std::collections::{HashMap, HashSet};
    let dst_hash: HashMap<&str, &str> = dst
        .iter()
        .map(|e| (e.path.as_str(), e.sha256.as_str()))
        .collect();
    let src_paths: HashSet<&str> = src.iter().map(|e| e.path.as_str()).collect();
    let mut transfer: Vec<String> = src
        .iter()
        .filter(|e| {
            dst_hash
                .get(e.path.as_str())
                .map(|h| *h != e.sha256.as_str())
                .unwrap_or(true)
        })
        .map(|e| e.path.clone())
        .collect();
    let mut extraneous: Vec<String> = dst
        .iter()
        .filter(|e| !src_paths.contains(e.path.as_str()))
        .map(|e| e.path.clone())
        .collect();
    transfer.sort();
    extraneous.sort();
    (transfer, extraneous)
}

/// Tar+gzip a specific list of relative paths (incremental push body / pull
/// export). Skips missing or unsafe paths. Enforces `MAX_SYNC_BYTES`.
pub fn tar_paths(root: &Path, paths: &[String]) -> Result<Vec<u8>, String> {
    let mut total = 0u64;
    let mut valid: Vec<&str> = Vec::new();
    for p in paths {
        let rel = Path::new(p);
        if is_unsafe_entry(rel) {
            continue;
        }
        if let Ok(m) = std::fs::metadata(root.join(rel)) {
            if m.is_file() {
                total += m.len();
                valid.push(p);
            }
        }
    }
    if total > MAX_SYNC_BYTES {
        return Err(format!(
            "changed files total {} MB, over the {} MB sync cap",
            total / 1_048_576,
            MAX_SYNC_BYTES / 1_048_576
        ));
    }
    let enc = GzEncoder::new(Vec::new(), Compression::default());
    let mut tar = tar::Builder::new(enc);
    for p in valid {
        let abs = root.join(p);
        let mut f =
            std::fs::File::open(&abs).map_err(|e| format!("open {}: {}", abs.display(), e))?;
        tar.append_file(p, &mut f)
            .map_err(|e| format!("tar append {}: {}", p, e))?;
    }
    let enc = tar.into_inner().map_err(|e| format!("tar finish: {}", e))?;
    enc.finish().map_err(|e| format!("gz finish: {}", e))
}

/// Move a list of relative paths to `.sync-trash/<ts>/` (incremental `--delete`:
/// the partial tarball is applied via `untar_workspace(.., delete=false)`, then
/// the extraneous paths are trashed with this).
pub fn trash_paths(root: &Path, paths: &[String]) -> Result<UntarResult, String> {
    let root = canonical_root(root)?;
    let trash = root.join(TRASH_PREFIX).join(unix_secs().to_string());
    let mut trash_used = false;
    let mut deleted = 0usize;
    for p in paths {
        let rel = Path::new(p);
        if is_unsafe_entry(rel) {
            continue;
        }
        if root.join(rel).exists() {
            move_to_trash(&root, &trash, rel, &mut trash_used)?;
            deleted += 1;
        }
    }
    Ok(UntarResult {
        written: 0,
        deleted,
        trash_dir: trash_used.then_some(trash),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let p = std::env::temp_dir().join(format!("wssync-{tag}-{ts}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn roundtrip_and_strip() {
        let src = tmp("src");
        write(&src, "a.txt", "hello");
        write(&src, "sub/b.md", "world");
        write(&src, ".thclaws/settings.json", "{}");
        write(&src, ".thclaws/sessions/x.jsonl", "RUNTIME"); // stripped
        let bytes = tar_workspace(&src, false).unwrap();
        let dst = tmp("dst");
        let r = untar_workspace(&bytes, &dst, false).unwrap();
        assert_eq!(r.written, 3); // a.txt, sub/b.md, settings.json
        assert_eq!(std::fs::read_to_string(dst.join("a.txt")).unwrap(), "hello");
        assert_eq!(
            std::fs::read_to_string(dst.join("sub/b.md")).unwrap(),
            "world"
        );
        assert!(
            !dst.join(".thclaws/sessions/x.jsonl").exists(),
            "runtime must be stripped"
        );
    }

    #[test]
    fn delete_moves_extraneous_to_trash() {
        let dst = tmp("del");
        write(&dst, "keep.txt", "v1");
        write(&dst, "stale.txt", "old"); // not in tarball → should be trashed
        let src = tmp("delsrc");
        write(&src, "keep.txt", "v2");
        let bytes = tar_workspace(&src, false).unwrap();
        let r = untar_workspace(&bytes, &dst, true).unwrap();
        assert_eq!(std::fs::read_to_string(dst.join("keep.txt")).unwrap(), "v2");
        assert!(!dst.join("stale.txt").exists(), "extraneous removed");
        assert_eq!(r.deleted, 1);
        let trash = r.trash_dir.expect("trash created");
        assert_eq!(
            std::fs::read_to_string(trash.join("stale.txt")).unwrap(),
            "old",
            "recoverable in trash"
        );
    }

    #[test]
    fn no_delete_keeps_extraneous() {
        let dst = tmp("nodel");
        write(&dst, "stale.txt", "old");
        let src = tmp("nodelsrc");
        write(&src, "a.txt", "x");
        let bytes = tar_workspace(&src, false).unwrap();
        let r = untar_workspace(&bytes, &dst, false).unwrap();
        assert!(
            dst.join("stale.txt").exists(),
            "without --delete, extraneous stays"
        );
        assert_eq!(r.deleted, 0);
    }

    #[test]
    fn is_empty_treats_bare_settings_as_empty() {
        let root = tmp("empty");
        write(&root, ".thclaws/settings.json", "{}");
        assert!(is_empty(&root).unwrap());
        write(&root, "real.txt", "x");
        assert!(!is_empty(&root).unwrap());
    }

    #[test]
    fn binding_roundtrip() {
        let root = tmp("bind");
        let b = Binding {
            workspace_id: Some("ws-123".into()),
            slug: Some("my-agent".into()),
            ..Default::default()
        };
        write_binding(&root, &b).unwrap();
        assert_eq!(read_binding(&root).workspace_id.as_deref(), Some("ws-123"));
    }

    #[test]
    fn rejects_path_traversal() {
        // The safe tar Builder refuses to even write a `..` entry, so test the
        // guard predicate directly — it's what untar enforces on every entry.
        assert!(is_unsafe_entry(Path::new("../escape.txt")));
        assert!(is_unsafe_entry(Path::new("a/../../escape")));
        assert!(is_unsafe_entry(Path::new("/etc/passwd")));
        assert!(!is_unsafe_entry(Path::new("ok/rel.txt")));
        assert!(!is_unsafe_entry(Path::new("a/b/c.md")));
    }

    #[test]
    fn manifest_and_diff() {
        let a = tmp("mfa");
        write(&a, "same.txt", "x");
        write(&a, "changed.txt", "v1");
        write(&a, "only_a.txt", "a");
        let b = tmp("mfb");
        write(&b, "same.txt", "x");
        write(&b, "changed.txt", "v2");
        write(&b, "only_b.txt", "b");
        let (transfer, extraneous) =
            diff(&build_manifest(&a).unwrap(), &build_manifest(&b).unwrap());
        assert_eq!(
            transfer,
            vec!["changed.txt".to_string(), "only_a.txt".to_string()]
        );
        assert_eq!(extraneous, vec!["only_b.txt".to_string()]);
    }

    #[test]
    fn incremental_push_apply() {
        let dst = tmp("incdst");
        write(&dst, "keep.txt", "old");
        write(&dst, "stale.txt", "bye");
        let src = tmp("incsrc");
        write(&src, "keep.txt", "new");
        write(&src, "added.txt", "hi");
        let (transfer, extraneous) = diff(
            &build_manifest(&src).unwrap(),
            &build_manifest(&dst).unwrap(),
        );
        let tarball = tar_paths(&src, &transfer).unwrap();
        let w = untar_workspace(&tarball, &dst, false).unwrap();
        let t = trash_paths(&dst, &extraneous).unwrap();
        assert_eq!(
            std::fs::read_to_string(dst.join("keep.txt")).unwrap(),
            "new"
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("added.txt")).unwrap(),
            "hi"
        );
        assert!(!dst.join("stale.txt").exists(), "extraneous removed");
        assert_eq!(w.written, 2);
        assert_eq!(t.deleted, 1);
    }
}
