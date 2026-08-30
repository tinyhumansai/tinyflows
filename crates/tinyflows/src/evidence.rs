//! Bounding what a run hands back to a reader that cannot take all of it.
//!
//! A node's output is whatever it produced — a 10 MB HTTP body is a perfectly
//! legal item. That is fine in memory and fatal everywhere it is *reported*: a
//! durable run record grows until every future history listing reads an
//! arbitrarily large file, and a trace projected for a model puts the same
//! payload in a context window a hundred times over.
//!
//! So evidence is bounded at the boundary, never in the engine. Execution and
//! diagnosis keep the full in-memory value; only the copy handed outward is
//! summarized, and it is summarized into a wrapper that says so rather than a
//! silently shortened value.
//!
//! The wrapper shape is deliberately identical at every budget, so a reader
//! that can unpack a truncated run record already knows how to unpack a
//! truncated trace.
//!
//! ```
//! use tinyflows::evidence::{bounded_within, is_truncated};
//! use serde_json::json;
//!
//! let small = json!({ "ok": true });
//! assert_eq!(bounded_within(&small, 1024), small);
//! assert!(!is_truncated(&small));
//!
//! let large = json!({ "body": "x".repeat(4096) });
//! let bounded = bounded_within(&large, 1024);
//! assert!(is_truncated(&bounded));
//! ```

/// Maximum serialized bytes retained for one step input or output.
pub(crate) const MAX_EVIDENCE_BYTES: usize = 64 * 1024;

/// The key marking a value that was bounded rather than stored whole.
///
/// Part of the on-disk format, so it is a named constant rather than a literal:
/// a reader that looks for the wrong string does not fail loudly, it silently
/// renders a truncation wrapper as if it were the value.
pub const TRUNCATED_KEY: &str = "_flowsTruncated";

/// The key a sibling host wrote before this bounding moved into the engine
/// crate.
///
/// Run records are written once and never revised, so files carrying it exist
/// and will keep existing. Recognising it costs one comparison; not recognising
/// it would make every one of those records read as an untruncated object whose
/// only fields are `originalBytes` and `preview`.
pub const LEGACY_TRUNCATED_KEY: &str = "_medullaTruncated";

/// Whether `value` is a truncation wrapper rather than a stored value.
///
/// Accepts both [`TRUNCATED_KEY`] and [`LEGACY_TRUNCATED_KEY`], so a host reads
/// its own history back regardless of which build wrote it.
#[must_use]
pub fn is_truncated(value: &serde_json::Value) -> bool {
    [TRUNCATED_KEY, LEGACY_TRUNCATED_KEY]
        .iter()
        .any(|key| value.get(key).and_then(serde_json::Value::as_bool) == Some(true))
}

/// Keep small evidence intact and summarize values that would bloat history.
///
/// Execution and diagnosis retain the engine's full in-memory value. Only the
/// durable inspection copy is bounded, so one response cannot make every
/// future history listing read an arbitrarily large file.
#[must_use]
pub fn bounded_evidence(value: &serde_json::Value) -> serde_json::Value {
    bounded_within(value, MAX_EVIDENCE_BYTES)
}

/// Keep small values intact and summarize ones larger than `max_bytes`.
///
/// The same bounding as [`bounded_evidence`] against a caller-chosen budget.
/// The durable record uses a generous one because it is written once; a reply
/// projected for a model uses a much smaller one, because a hundred of them
/// land in the same context window.
///
/// The wrapper shape is deliberately identical at every budget, so a reader
/// that knows how to unpack a truncated run file already knows how to unpack a
/// truncated reply.
#[must_use]
pub fn bounded_within(value: &serde_json::Value, max_bytes: usize) -> serde_json::Value {
    let serialized = serde_json::to_string(value).unwrap_or_else(|_| value.to_string());
    if serialized.len() <= max_bytes {
        return value.clone();
    }
    // The preview is itself embedded in JSON, so reserve half the budget for
    // escaping plus the wrapper metadata. Quotes and backslashes can nearly
    // double when serialized a second time.
    let preview_budget = (max_bytes / 2).saturating_sub(256);
    let end = serialized
        .char_indices()
        .map(|(index, _)| index)
        .take_while(|index| *index <= preview_budget)
        .last()
        .unwrap_or(0);
    let bounded = serde_json::json!({
        TRUNCATED_KEY: true,
        "originalBytes": serialized.len(),
        "preview": &serialized[..end],
    });
    debug_assert!(
        serde_json::to_vec(&bounded)
            .map(|body| body.len() <= max_bytes.max(512))
            .unwrap_or(false)
    );
    bounded
}
