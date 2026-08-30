//! File-backed [`Checkpointer`] — a durable JSON/JSONL backend that survives
//! process restarts.
//!
//! Each thread maps to one append-only JSONL file under a base directory: one
//! checkpoint record (a serialized [`Checkpoint`]) per line, written in
//! insertion order. Reads stream the thread file line by line; deletes rewrite
//! (or remove) it; [`Checkpointer::copy_thread`] copies a thread's file while
//! rewriting only the `thread_id` on each record, so the parent lineage spine is
//! preserved exactly as in memory.
//!
//! The backend is generic over `State`, but only requires
//! `State: Serialize + DeserializeOwned` on the [`Checkpointer`] impl block — the
//! trait itself stays bound-free so the in-memory path keeps working for states
//! that are not serializable.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::marker::PhantomData;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use serde::Serialize;
use serde::de::DeserializeOwned;

/// Minimal projection used to read a checkpoint's id without deserializing its
/// `State` payload, so `get` can pick the target line and decode only that one.
#[derive(serde::Deserialize)]
struct CheckpointIdHeader {
    checkpoint_id: String,
}

use super::{
    Checkpoint, CheckpointConfig, CheckpointMetadata, CheckpointTuple, Checkpointer, PendingWrite,
    merge_writes,
};
use crate::graph::error::{GraphError, Result};
use crate::graph::ids::CheckpointId;

/// File extension for per-thread checkpoint logs.
const THREAD_EXT: &str = "jsonl";

/// Filename suffix for a thread's **pending-writes** sidecar.
///
/// Writes are recorded after their checkpoint is already durable, so they
/// cannot live in the append-only checkpoint log without turning it into a
/// mixed-record format that every reader would have to discriminate. A sibling
/// file keeps the checkpoint log exactly as it was.
const WRITES_SUFFIX: &str = ".writes.jsonl";

/// Process-wide counter making temp-file names unique so concurrent atomic
/// rewrites of the same thread never collide on their scratch file.
static TMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One line of a thread's pending-writes sidecar: the write plus the
/// `(namespace, checkpoint_id)` it is filed under.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct WriteRecord {
    #[serde(default)]
    namespace: Vec<String>,
    checkpoint_id: String,
    write: PendingWrite,
}

/// A [`Checkpointer`] that persists checkpoints as JSONL files under a base
/// directory, one file per thread.
///
/// Cheap to clone; clones address the same base directory. The base directory
/// is created lazily on the first write.
pub struct FileCheckpointer<State> {
    base_dir: PathBuf,
    _marker: PhantomData<fn() -> State>,
}

impl<State> FileCheckpointer<State> {
    /// Creates a checkpointer rooted at `base_dir`.
    ///
    /// The directory is not touched until the first checkpoint is written.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
            _marker: PhantomData,
        }
    }

    /// Returns the base directory backing this checkpointer.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Resolves the JSONL file path for `thread_id`.
    ///
    /// The thread id is percent-escaped so it is a safe, injective single path
    /// component (no separators, no collisions between distinct ids).
    fn thread_path(&self, thread_id: &str) -> PathBuf {
        let canonical = self.canonical_thread_path(thread_id);
        if canonical.exists() {
            canonical
        } else {
            let legacy = self.legacy_thread_path(thread_id);
            if legacy.exists() { legacy } else { canonical }
        }
    }

    /// Resolves the pending-writes sidecar path for `thread_id`.
    fn writes_path(&self, thread_id: &str) -> PathBuf {
        let canonical = self.canonical_writes_path(thread_id);
        if canonical.exists() {
            canonical
        } else {
            let legacy = self.legacy_writes_path(thread_id);
            if legacy.exists() { legacy } else { canonical }
        }
    }

    fn canonical_thread_path(&self, thread_id: &str) -> PathBuf {
        self.base_dir
            .join(format!("{}.{THREAD_EXT}", escape_thread_id(thread_id)))
    }

    fn canonical_writes_path(&self, thread_id: &str) -> PathBuf {
        self.base_dir
            .join(format!("{}{WRITES_SUFFIX}", escape_thread_id(thread_id)))
    }

    fn legacy_thread_path(&self, thread_id: &str) -> PathBuf {
        self.base_dir.join(format!(
            "{}.{THREAD_EXT}",
            legacy_escape_thread_id(thread_id)
        ))
    }

    fn legacy_writes_path(&self, thread_id: &str) -> PathBuf {
        self.base_dir.join(format!(
            "{}{WRITES_SUFFIX}",
            legacy_escape_thread_id(thread_id)
        ))
    }

    /// Reads a thread's write sidecar, tolerating a torn trailing line exactly
    /// as [`FileCheckpointer::read_records`] does.
    fn read_write_records(path: &Path, thread_id: &str) -> Result<Vec<WriteRecord>> {
        let text = match fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err("open writes file", e)),
        };
        decode_lines(&text, &format!("writes for thread `{thread_id}`"), |line| {
            serde_json::from_str::<WriteRecord>(line)
        })
    }
}

impl<State> Clone for FileCheckpointer<State> {
    fn clone(&self) -> Self {
        Self {
            base_dir: self.base_dir.clone(),
            _marker: PhantomData,
        }
    }
}

/// Percent-escapes any byte outside `[a-z0-9._-]` so a thread id maps to a
/// single filename component that is injective **even on a case-insensitive
/// filesystem**.
///
/// # Why uppercase is escaped
///
/// The obvious safe set is `[A-Za-z0-9._-]`, and that is what this used to use.
/// It is injective on a case-*sensitive* filesystem and silently is not on
/// APFS, HFS+ or NTFS: threads `"Alice"` and `"alice"` map to `Alice.jsonl` and
/// `alice.jsonl`, which are the *same file*. Two unrelated runs then append into
/// one lineage, and reads hand each of them the other's checkpoints.
///
/// Escaping `A-Z` fixes it while staying case-*preserving* (the id is still
/// recoverable byte-for-byte from the name). The only uppercase characters left
/// in the output are the hex digits `A-F` of an escape, and escapes are always
/// emitted as `%` + exactly two uppercase hex digits, so no two outputs can
/// differ only by letter case: lowercasing the whole name is injective on the
/// image, which is exactly what case-insensitive collision-freedom means.
///
fn escape_thread_id(thread_id: &str) -> String {
    let mut out = String::with_capacity(thread_id.len());
    for &b in thread_id.as_bytes() {
        if b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

/// The filename escaping used before uppercase letters were made explicit.
/// Kept only as a read/write fallback so persisted threads remain reachable
/// after an upgrade; new thread files always use [`escape_thread_id`].
fn legacy_escape_thread_id(thread_id: &str) -> String {
    let mut out = String::with_capacity(thread_id.len());
    for &b in thread_id.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-') {
            out.push(b as char);
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

fn io_err(context: &str, err: impl std::fmt::Display) -> GraphError {
    GraphError::Checkpoint(format!("file checkpointer: {context}: {err}"))
}

/// Builds a [`CheckpointTuple`] from an owned checkpoint, mirroring the
/// addressing/parent/pending-writes wiring of the default
/// [`Checkpointer::get_tuple`].
fn tuple_from_checkpoint<State>(checkpoint: Checkpoint<State>) -> CheckpointTuple<State> {
    let config = CheckpointConfig {
        thread_id: checkpoint.thread_id.clone(),
        checkpoint_id: Some(checkpoint.checkpoint_id.clone()),
        namespace: checkpoint.namespace.clone(),
    };
    let parent_config = checkpoint
        .parent_checkpoint_id
        .as_ref()
        .map(|parent| CheckpointConfig {
            thread_id: checkpoint.thread_id.clone(),
            checkpoint_id: Some(parent.clone()),
            namespace: checkpoint.namespace.clone(),
        });
    let pending_writes = checkpoint.pending_writes.clone();
    CheckpointTuple {
        config,
        checkpoint,
        parent_config,
        pending_writes,
    }
}

impl<State> FileCheckpointer<State>
where
    State: DeserializeOwned,
{
    /// Reads every record in `thread_id`'s file, in insertion order.
    ///
    /// Returns an empty vec when the thread file does not exist.
    fn read_records(&self, thread_id: &str) -> Result<Vec<Checkpoint<State>>> {
        let path = self.thread_path(thread_id);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err("open thread file", e)),
        };
        decode_lines(&text, &format!("thread `{thread_id}`"), |line| {
            serde_json::from_str::<Checkpoint<State>>(line)
        })
    }
}

/// Decodes one JSON object per line, tolerating a **torn trailing line**.
///
/// A crash between `write_all` and the OS flushing the tail of the buffer
/// leaves a partial final line. It can only ever be the last one — the file is
/// append-only — so that is the only line whose decode failure is forgiven, and
/// only when the file does not end in a newline (a complete record always
/// does). Anything else is real corruption and still errors.
///
/// This matters more than "one lost record": the previous behaviour failed the
/// whole read, so a single torn byte made a thread permanently unreadable, with
/// no way to get at the hundreds of intact checkpoints in front of it.
fn decode_lines<T, F>(text: &str, what: &str, mut decode: F) -> Result<Vec<T>>
where
    F: FnMut(&str) -> std::result::Result<T, serde_json::Error>,
{
    let complete = text.is_empty() || text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let last_index = lines.len().saturating_sub(1);
    let mut out = Vec::with_capacity(lines.len());
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match decode(line) {
            Ok(record) => out.push(record),
            Err(e) if !complete && i == last_index => {
                tracing::warn!(
                    "[checkpoint:file] {what}: discarding torn trailing line \
                     ({} bytes, no terminating newline): {e}",
                    line.len()
                );
            }
            Err(e) => return Err(io_err("decode record", e)),
        }
    }
    Ok(out)
}

impl<State> FileCheckpointer<State>
where
    State: Serialize,
{
    /// Overwrites `thread_id`'s file with `records` (one JSON line each).
    ///
    /// When `records` is empty the file is removed so empty threads disappear
    /// from [`Checkpointer::list_threads`].
    fn write_records(&self, thread_id: &str, records: &[Checkpoint<State>]) -> Result<()> {
        let path = self.thread_path(thread_id);
        if records.is_empty() {
            match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(io_err("remove empty thread file", e)),
            }
        } else {
            fs::create_dir_all(&self.base_dir).map_err(|e| io_err("create base dir", e))?;
            let mut buf = String::new();
            for record in records {
                let line = serde_json::to_string(record).map_err(|e| io_err("encode record", e))?;
                buf.push_str(&line);
                buf.push('\n');
            }
            write_atomic(&path, buf.as_bytes())
        }
    }
}

/// Writes `bytes` to `path` atomically: a uniquely named temp file in the same
/// directory, fsynced, then renamed over the destination.
///
/// The prune/delete path used to rewrite the thread file **in place** with
/// `fs::write`, which truncates first: a crash anywhere in the following write
/// leaves a truncated or empty file, and the whole history is gone — not the
/// pruned tail, all of it. Rename is atomic for same-directory paths on POSIX
/// and Windows, so a reader sees either the old file or the new one, and a
/// crash leaves the old one intact. This is the same shape `FileStore::put`
/// already uses.
fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().ok_or_else(|| {
        GraphError::Checkpoint(format!(
            "file checkpointer: path has no parent directory: {}",
            path.display()
        ))
    })?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("thread");
    let tmp = dir.join(format!(
        "{file_name}.tmp.{}.{}",
        std::process::id(),
        TMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let write_and_sync = || -> std::io::Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()
    };
    if let Err(e) = write_and_sync() {
        let _ = fs::remove_file(&tmp);
        return Err(io_err("write temp thread file", e));
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(io_err("rename temp thread file", e));
    }
    Ok(())
}

mod checkpointer;
