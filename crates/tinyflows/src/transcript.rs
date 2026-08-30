//! One line of what an agent did, as a run record keeps it.
//!
//! An `agent` node runs a host's harness, and a harness has an event stream:
//! it thinks, calls a tool, reads the result, thinks again. The node's output
//! says what came *out* of that; a transcript says what happened *inside* it,
//! so a run read back tomorrow explains itself rather than only passing or
//! failing.
//!
//! Two surfaces carry these, and both are the engine's:
//! [`AgentRunOutcome::transcript`](crate::caps::AgentRunOutcome::transcript),
//! where a host hands them over, and
//! [`ExecutionStep::transcript`](crate::observability::ExecutionStep::transcript),
//! where the engine hands them back to a [`RunObserver`](crate::observability::RunObserver).
//! A host that persists runs also finds them on `store::types::RunStep`.
//!
//! **Nothing in this crate folds a host's event stream into these** — the
//! engine has no event stream of its own, and what counts as one entry is a
//! judgement only the harness can make. Hosts fold; the crate carries.
//!
//! **Settled, not live.** Entries ride the outcome a harness returns, so they
//! reach an observer when the node finishes rather than as they happen.
//! Reporting them during a run would need a sink on the agent capability, and
//! [`AgentRunRequest`](crate::caps::AgentRunRequest) cannot carry one — it is
//! `Serialize` + `PartialEq` — so that is a deliberate follow-up.
//!
//! Deliberately flat and stringly-typed. Mirroring a host's own event
//! vocabulary into the record would make every event kind it adds later a
//! breaking change to a file format that must stay readable by older builds. A
//! reader meeting an unfamiliar `kind` still has a timestamp and a line of text
//! to render.

use serde::{Deserialize, Serialize};

/// Bytes of one entry's `text`, marker included.
///
/// A clipped entry is at most this long *in total* — the truncation marker
/// is charged against the budget, not added after it.
///
/// A stored run bounds step `input`, `output` and its own `inputs` so no
/// single value can grow a run record without limit; a transcript entry is
/// the same kind of host-produced text (a tool result, a model message) and
/// needs the same ceiling. Small on purpose — a transcript is many short
/// lines, not one large payload, and a step with hundreds of entries must
/// not let one long entry become the whole record's size budget.
///
/// A host with a genuinely large payload — a full tool result, a reasoning
/// body — keeps it in its own store and leaves the entry as the index line
/// pointing at it. That split is why this ceiling can stay small.
pub const MAX_ENTRY_TEXT_BYTES: usize = 4 * 1024;

/// Appended to a clipped entry, and counted against
/// [`MAX_ENTRY_TEXT_BYTES`] rather than added on top of it.
const TRUNCATION_MARKER: &str = " …[truncated]";

/// One thing an agent did, in the order it did it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptEntry {
    /// Epoch milliseconds, as the host stamped the event.
    pub at_ms: i64,
    /// The host event kind this was folded from — `agent_message`, `tool_call`,
    /// `tool_result`, `agent_thinking`, `error`, and so on.
    ///
    /// Carried verbatim rather than mapped to a closed set, so a kind added to
    /// a host's wire vocabulary later shows up here without a change to this
    /// file.
    pub kind: String,
    /// The renderable line: the message text, the tool's one-line summary, the
    /// error message.
    pub text: String,
}

impl TranscriptEntry {
    /// Build an entry with `text` capped at [`MAX_ENTRY_TEXT_BYTES`].
    ///
    /// Nothing in this crate folds a host's event stream into these — that
    /// happens entirely on the host side, as the module doc says — so this is
    /// the bound a host's folding code is expected to apply per entry, the way
    /// a stored run bounds the record's other host-produced text.
    #[must_use]
    pub fn bounded(at_ms: i64, kind: impl Into<String>, text: impl Into<String>) -> Self {
        let mut text = text.into();
        if text.len() > MAX_ENTRY_TEXT_BYTES {
            // Reserve the marker's own bytes BEFORE choosing the cut, so the
            // finished entry honours the cap rather than exceeding it by the
            // length of the thing announcing the cut.
            let budget = MAX_ENTRY_TEXT_BYTES - TRUNCATION_MARKER.len();
            let end = text
                .char_indices()
                .map(|(index, _)| index)
                .take_while(|index| *index <= budget)
                .last()
                .unwrap_or(0);
            text.truncate(end);
            text.push_str(TRUNCATION_MARKER);
        }
        Self {
            at_ms,
            kind: kind.into(),
            text,
        }
    }
}
