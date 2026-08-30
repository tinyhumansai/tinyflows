//! The call log [`MockCaps`](super::MockCaps) and its [`Double`](super::double)
//! write to: [`CallOutcome`], [`CapCall`], and [`CallLog`] itself.
//!
//! Split out of `mocks.rs` (and out of `mocks_double.rs`, which is itself a
//! split of `mocks.rs`) to keep every file under the repository's
//! line-length limit.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::glob_matches;

/// How one capability call ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallOutcome {
    /// The call returned a value.
    Ok(Value),
    /// The call failed, with this message.
    Err(String),
}

/// One capability call a run made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapCall {
    /// Position in the run's single call sequence, from 0.
    ///
    /// One counter across *all* capabilities, so the log says what order things
    /// happened in — which per-capability counters cannot.
    pub seq: u64,
    /// Which capability — see the [`capability`](super::capability) constants.
    pub capability: String,
    /// The trait method (`invoke`, `complete`, `request`, …).
    pub method: String,
    /// The node that made the call.
    ///
    /// `None` only when the call was made outside a node activation, which no
    /// engine path does today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<String>,
    /// What identifies the target within the capability: a tool slug, an agent
    /// ref, an HTTP method and URL, a state key. Empty when the capability has
    /// no such notion.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub target: String,
    /// The arguments the call was made with.
    pub args: Value,
    /// What it returned.
    pub outcome: CallOutcome,
}

/// Every capability call a run made, in order.
///
/// Shared by every double in one [`MockCaps`](super::MockCaps), so the
/// ordering across capabilities is real rather than assembled afterwards from
/// separate logs.
#[derive(Debug, Default)]
pub struct CallLog {
    calls: Mutex<Vec<CapCall>>,
    next_seq: AtomicU64,
}

impl CallLog {
    /// An empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a call, assigning it the next sequence number.
    pub(super) fn record(
        &self,
        capability: &str,
        method: &str,
        node_id: Option<String>,
        target: String,
        args: Value,
        outcome: CallOutcome,
    ) {
        let call = CapCall {
            seq: self.next_seq.fetch_add(1, Ordering::SeqCst),
            capability: capability.to_string(),
            method: method.to_string(),
            node_id,
            target,
            args,
            outcome,
        };
        self.calls.lock().expect("call log poisoned").push(call);
    }

    /// Every call recorded so far, in sequence order.
    #[must_use]
    pub fn calls(&self) -> Vec<CapCall> {
        let mut calls = self.calls.lock().expect("call log poisoned").clone();
        calls.sort_by_key(|call| call.seq);
        calls
    }

    /// The calls matching a capability and an optional target glob.
    ///
    /// `capability` is one of the [`capability`](super::capability) constants; `target` accepts the
    /// same `*` globbing the rules do, and `None` matches every target.
    #[must_use]
    pub fn matching(&self, capability: &str, target: Option<&str>) -> Vec<CapCall> {
        self.calls()
            .into_iter()
            .filter(|call| call.capability == capability)
            .filter(|call| target.is_none_or(|glob| glob_matches(glob, &call.target)))
            .collect()
    }

    /// How many calls match — the count an assertion usually wants.
    #[must_use]
    pub fn count(&self, capability: &str, target: Option<&str>) -> usize {
        self.matching(capability, target).len()
    }
}
