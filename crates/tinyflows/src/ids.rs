//! Random tokens, for the places a name has to be unique rather than meaningful.
//!
//! Scratch filenames, minted note and proposal ids, the suffix on a quarantined
//! document. None of these are read for their content; what they have to
//! guarantee is that two writers — in this process or in another one sharing the
//! same directory — never choose the same one.
//!
//! Random rather than sequential for exactly that reason: a counter separates
//! writers inside one process and collides immediately across two. A dedicated
//! UUID dependency would do the same job, but this crate already carries
//! `getrandom` for the engine, and an opaque token needs no version, variant, or
//! canonical formatting.

use std::sync::atomic::{AtomicU64, Ordering};

/// Bytes of randomness behind one token — the same 128 bits a v4 UUID carries.
const TOKEN_BYTES: usize = 16;

/// Fallback sequence, used only when the OS refuses randomness.
static SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A unique, opaque token: 32 lowercase hex characters.
///
/// Safe as a path component by construction — hex has no separators, no
/// `..`, and no case-folding surprises.
///
/// Falls back to `<pid>-<counter>` if the OS refuses randomness at all, a case
/// that should not happen and where a within-process guarantee still beats a
/// fixed name shared by every writer.
#[must_use]
pub(crate) fn token() -> String {
    let mut bytes = [0u8; TOKEN_BYTES];
    if getrandom::fill(&mut bytes).is_ok() {
        let mut out = String::with_capacity(TOKEN_BYTES * 2);
        for byte in bytes {
            out.push_str(&format!("{byte:02x}"));
        }
        return out;
    }
    format!(
        "{}-{}",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
#[path = "ids_tests.rs"]
mod tests;
