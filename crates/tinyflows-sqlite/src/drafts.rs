//! File-based persistence for [`FlowDraft`]s.
//!
//! Drafts are plain JSON files under `<dir>/drafts/<id>.json`, one file per
//! draft — deliberately NOT a SQLite table (no schema, no migration, trivially
//! inspectable and deletable). A draft is the shared working copy an authoring
//! agent and a canvas both read and write by id across turns and reloads, which
//! rules out any client-only storage.
//!
//! This module is the storage layer only. Promotion policy — what a draft has
//! to satisfy before it becomes a saved [`tinyflows_catalog::Flow`] — is the
//! host's, and stays there.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use serde_json::Value;
use uuid::Uuid;

use tinyflows_catalog::{DraftOrigin, FlowDraft};

/// The directory holding draft files, `<dir>/drafts`.
fn drafts_dir(dir: &Path) -> PathBuf {
    dir.join("drafts")
}

/// Whether `id` is a safe draft-file stem — guards `get`/`update`/`delete`
/// against path traversal (`..`, separators) since the id reaches the
/// filesystem. Server-minted ids are UUIDs; this only accepts that shape.
fn is_safe_draft_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// The on-disk path for draft `id` (validated).
fn draft_path(dir: &Path, id: &str) -> Result<PathBuf> {
    if !is_safe_draft_id(id) {
        bail!("invalid draft id: {id:?}");
    }
    Ok(drafts_dir(dir).join(format!("{id}.json")))
}

/// Creates a new draft, writes it to disk, and returns it.
pub fn create_draft(
    dir: &Path,
    flow_id: Option<String>,
    name: String,
    graph: Value,
    origin: DraftOrigin,
) -> Result<FlowDraft> {
    let now = Utc::now().to_rfc3339();
    let draft = FlowDraft {
        id: Uuid::new_v4().to_string(),
        flow_id,
        name,
        graph,
        origin,
        created_at: now.clone(),
        updated_at: now,
    };
    write_draft(dir, &draft)?;
    tracing::debug!(
        target: "flows",
        draft_id = %draft.id,
        origin = draft.origin.as_str(),
        "[flows] draft_store: created draft"
    );
    Ok(draft)
}

/// Reads a draft by id, or `None` if no such file exists.
pub fn get_draft(dir: &Path, id: &str) -> Result<Option<FlowDraft>> {
    let path = draft_path(dir, id)?;
    match std::fs::read(&path) {
        Ok(bytes) => {
            let draft: FlowDraft =
                serde_json::from_slice(&bytes).with_context(|| format!("draft {id} is corrupt"))?;
            Ok(Some(draft))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading draft {id}")),
    }
}

/// Per-draft-file locks, so two concurrent `update_draft` calls against the
/// SAME draft (the canvas and the authoring agent both patch it) serialize
/// their read-modify-write instead of racing: both would otherwise read the
/// same on-disk version, apply different fields, and whichever `write_draft`
/// lands last silently drops the other's change. Keyed by path rather than a
/// single global lock so unrelated drafts still update concurrently.
///
/// This is a **process-local** lock — it closes the race for the common case
/// of one host process handling both the canvas and the agent's RPCs, which
/// is how this crate is embedded today. It does not protect against two
/// separate OS processes writing the same draft file; that would need an
/// OS-level advisory lock (`fs2`, as the sibling `store` docs elsewhere in
/// this repo already do for their own file) or a CAS check against
/// `updated_at`, either of which is a larger change than this crate's
/// internal restructuring should make unasked.
static DRAFT_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Weak<Mutex<()>>>>> = OnceLock::new();

/// Returns the shared lock for `path`, creating one on first use. The registry
/// keeps only weak references and prunes inactive paths on every lookup, so a
/// process that sees many draft ids does not retain one allocation per id.
fn lock_for(path: &Path) -> Arc<Mutex<()>> {
    let registry = DRAFT_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry.lock().unwrap_or_else(|e| e.into_inner());
    registry.retain(|_, lock| lock.strong_count() > 0);
    if let Some(lock) = registry.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(Mutex::new(()));
    registry.insert(path.to_path_buf(), Arc::downgrade(&lock));
    lock
}

/// Patches a draft's mutable fields (any `Some` is applied), bumps
/// `updated_at`, persists, and returns the updated draft. Errors if the draft
/// does not exist.
pub fn update_draft(
    dir: &Path,
    id: &str,
    name: Option<String>,
    graph: Option<Value>,
    flow_id: Option<Option<String>>,
) -> Result<FlowDraft> {
    let path = draft_path(dir, id)?;
    let lock = lock_for(&path);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());

    let mut draft = get_draft(dir, id)?.with_context(|| format!("draft {id} not found"))?;
    if let Some(name) = name {
        draft.name = name;
    }
    if let Some(graph) = graph {
        draft.graph = graph;
    }
    if let Some(flow_id) = flow_id {
        draft.flow_id = flow_id;
    }
    draft.updated_at = Utc::now().to_rfc3339();
    write_draft(dir, &draft)?;
    tracing::debug!(target: "flows", draft_id = %id, "[flows] draft_store: updated draft");
    Ok(draft)
}

/// Lists all drafts, newest-updated first. Skips (and logs) any corrupt file
/// rather than failing the whole listing.
pub fn list_drafts(dir: &Path) -> Result<Vec<FlowDraft>> {
    let dir = drafts_dir(dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("listing drafts in {}", dir.display())),
    };
    let mut drafts = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        match std::fs::read(&path).map(|b| serde_json::from_slice::<FlowDraft>(&b)) {
            Ok(Ok(draft)) => drafts.push(draft),
            Ok(Err(e)) => {
                tracing::warn!(target: "flows", path = %path.display(), error = %e, "[flows] draft_store: skipping corrupt draft file");
            }
            Err(e) => {
                tracing::warn!(target: "flows", path = %path.display(), error = %e, "[flows] draft_store: could not read draft file");
            }
        }
    }
    // Newest-updated first (RFC3339 with a fixed +00:00 offset sorts lexically).
    drafts.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    Ok(drafts)
}

/// Deletes a draft file. Returns `true` if a file was removed, `false` if it
/// was already absent.
pub fn delete_draft(dir: &Path, id: &str) -> Result<bool> {
    let path = draft_path(dir, id)?;
    match std::fs::remove_file(&path) {
        Ok(()) => {
            tracing::debug!(target: "flows", draft_id = %id, "[flows] draft_store: deleted draft");
            Ok(true)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).with_context(|| format!("deleting draft {id}")),
    }
}

/// Serializes a draft to its file, creating the drafts dir if needed. Writes to
/// a temp file then renames, so a crash mid-write never leaves a corrupt draft.
fn write_draft(dir: &Path, draft: &FlowDraft) -> Result<()> {
    let drafts = drafts_dir(dir);
    std::fs::create_dir_all(&drafts)
        .with_context(|| format!("creating drafts dir {}", drafts.display()))?;
    let path = draft_path(dir, &draft.id)?;
    let tmp = drafts.join(format!(".{}.json.tmp", draft.id));
    let json = serde_json::to_vec_pretty(draft).context("serializing draft")?;
    std::fs::write(&tmp, &json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
#[path = "drafts_tests.rs"]
mod tests;
