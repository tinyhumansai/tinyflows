//! One workflow's notes on disk.
//!
//! Stored under the state directory beside run records rather than beside the
//! definitions, because a journal is *host* knowledge: it is what this machine
//! observed while running the workflow, not part of the document an operator
//! edits and commits.
//!
//! One file per workflow, not one per note. Notes are only ever read as a whole
//! set — a brief wants all of them or none — and a directory per workflow would
//! reproduce the unindexed scan that already makes run history expensive.

mod persistence;
mod prune;

pub use persistence::{append, list, supersede};

use crate::store::types::NoteId;

/// How many notes one workflow keeps.
///
/// Generous, because a note is a sentence rather than a graph, and a workflow
/// that has failed a hundred times has a hundred things worth remembering. The
/// cap exists so an automated pass writing on every failure cannot grow a file
/// without bound.
pub const MAX_NOTES: usize = 100;

/// Tie-breaker for notes written inside the same millisecond.
///
/// Process-wide for the same reason revisions use one: it only has to increase,
/// and a per-file count read off disk could be raced into reuse.
static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Mint a note id that sorts chronologically.
///
/// Same three-part scheme as a revision id: a zero-padded stamp so a lexical
/// sort is a chronological one, a monotonic counter because a pass writes
/// several notes inside one millisecond, and a random token because two
/// processes can pick the same counter.
pub fn mint_id(recorded_at: u64) -> NoteId {
    format!(
        "{recorded_at:013}-{:012}-{}",
        SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
        crate::ids::token()
    )
}

#[cfg(test)]
#[path = "journal_tests.rs"]
mod tests;
