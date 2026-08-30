//! Editing a workflow as a series of patches.
//!
//! Workflows here are written by agents as often as by people, and an agent
//! editing a graph by rewriting the whole JSON document loses information every
//! time it misremembers a field. The [`GraphOp`] patch language exists for
//! exactly this: small, named, checkable edits — add a node, merge-patch a
//! config, rewire an edge — that fail loudly rather than silently dropping what
//! they did not mention.
//!
//! Every edit here is apply → validate → gate → save, in that order, and a graph
//! that fails any of the three is never written. An author's mistake costs them
//! an error message, not their saved workflow.
//!
//! The gate is the store's own [`HostPolicy`](super::HostPolicy), reached
//! through [`WorkflowStore::policy`] rather than passed in, so an edit is always
//! judged by the rules of the store it is about to land in.

use std::sync::Arc;

use crate::graph_ops::{GraphOp, apply_ops};
use crate::model::WorkflowGraph;

use super::types::{WorkflowError, WorkflowRecord, record_fingerprint};
use super::{WorkflowStore, parse_workflow_with, require, validate_graph};

/// Apply `ops` to the workflow `id` and save the result.
///
/// Returns the saved record. The workflow is left untouched if any op fails to
/// apply or the result fails validation — the ops are applied to a copy, and
/// only a graph that would compile is written back.
pub fn apply_workflow_ops(
    store: &Arc<dyn WorkflowStore>,
    id: &str,
    ops: &[GraphOp],
) -> Result<WorkflowRecord, WorkflowError> {
    // A copilot, CLI, and another MCP process may all edit the same file through
    // independent store instances. Rebase the patch when another writer wins
    // between read and save instead of reporting two successes while silently
    // discarding the earlier edit.
    apply_workflow_ops_observed(store, id, ops, |_| {}).map(|(record, _)| record)
}

/// Apply graph operations while observing each freshly read save attempt.
///
/// The observer exists so a concurrency test can synchronize two writers after
/// their first read without relying on scheduler timing — including a host's
/// own test, pairing this against a writer of its own. Production callers use
/// [`apply_workflow_ops`], whose observer is a no-op.
pub fn apply_workflow_ops_observed(
    store: &Arc<dyn WorkflowStore>,
    id: &str,
    ops: &[GraphOp],
    observer: impl FnMut(usize),
) -> Result<(WorkflowRecord, usize), WorkflowError> {
    mutate_workflow_record(
        store,
        id,
        |record| {
            record.graph = apply_ops(&record.graph, ops)
                .map_err(|err| WorkflowError::Engine(format!("workflow '{id}': {err}")))?;
            validate_graph(id, &record.graph)?;
            store.policy().check_graph(id, &record.graph)
        },
        observer,
        "kept changing while the edit was being saved; retry the edit",
    )
}

/// Mutate and atomically save a workflow record, rebasing after CAS conflicts.
pub fn mutate_workflow_record(
    store: &Arc<dyn WorkflowStore>,
    id: &str,
    mut mutate: impl FnMut(&mut WorkflowRecord) -> Result<(), WorkflowError>,
    mut observer: impl FnMut(usize),
    failure: &str,
) -> Result<(WorkflowRecord, usize), WorkflowError> {
    const MAX_RETRIES: usize = 16;
    for attempt in 1..=MAX_RETRIES {
        let mut record = require(store.as_ref(), id)?;
        let expected = record_fingerprint(&record);
        mutate(&mut record)?;
        observer(attempt);
        if store.save_if_record_fingerprint(&record, &expected)? {
            return Ok((record, attempt));
        }
    }
    Err(WorkflowError::Engine(format!("workflow '{id}' {failure}")))
}

/// Apply `ops` only if the graph still matches `expected_fingerprint`.
///
/// Returns `None` when the expected fingerprint is stale, including when the
/// graph changes between the initial read and persistence. Returns `Some` only
/// after applying the ops, validating the graph and host semantic gates, and
/// durably saving the result.
///
/// # Errors
///
/// Returns an error when the workflow is missing, an op cannot be applied, the
/// resulting graph fails validation or semantic checks, or persistence fails.
pub fn apply_workflow_ops_if_unchanged(
    store: &Arc<dyn WorkflowStore>,
    id: &str,
    ops: &[GraphOp],
    expected_fingerprint: &str,
) -> Result<Option<WorkflowRecord>, WorkflowError> {
    let mut record = require(store.as_ref(), id)?;
    if super::types::fingerprint(&record.graph) != expected_fingerprint {
        return Ok(None);
    }
    // The caller only ever observed the *graph* fingerprint, so that stays the
    // freshness check above. The save itself guards the whole record: captured
    // right after this read and before the mutation below, so a concurrent
    // writer that changed only metadata (defaults, description — not the
    // graph) between our read and our save is still detected, instead of this
    // write silently overwriting that change with the metadata as it stood
    // when we read it.
    let observed = record_fingerprint(&record);
    record.graph = apply_ops(&record.graph, ops)
        .map_err(|err| WorkflowError::Engine(format!("workflow '{id}': {err}")))?;
    validate_graph(id, &record.graph)?;
    store.policy().check_graph(id, &record.graph)?;
    if !store.save_if_record_fingerprint(&record, &observed)? {
        return Ok(None);
    }
    Ok(Some(record))
}

/// Preview `ops` against the workflow `id` without saving.
///
/// The same checks as [`apply_workflow_ops`], minus the write. What an author
/// calls to see whether an edit is sound before committing to it.
pub fn preview_workflow_ops(
    store: &Arc<dyn WorkflowStore>,
    id: &str,
    ops: &[GraphOp],
) -> Result<WorkflowGraph, WorkflowError> {
    let record = require(store.as_ref(), id)?;
    let graph = apply_ops(&record.graph, ops)
        .map_err(|err| WorkflowError::Engine(format!("workflow '{id}': {err}")))?;
    validate_graph(id, &graph)?;
    store.policy().check_graph(id, &graph)?;
    Ok(graph)
}

/// Create a workflow from a whole graph document, replacing any existing one of
/// the same id.
///
/// Parses, then validates, then saves — the same order [`apply_workflow_ops`]
/// uses, and for the same reason. A document that parses is not necessarily a
/// graph the engine would compile, and a create path that skipped validation
/// would be the one way to get an unrunnable workflow into a store whose
/// listings are otherwise trustworthy.
pub fn create_workflow(
    store: &Arc<dyn WorkflowStore>,
    document: &str,
    id_fallback: &str,
) -> Result<WorkflowRecord, WorkflowError> {
    let record = parse_workflow_with(document, id_fallback, store.policy())
        .map_err(WorkflowError::Malformed)?;
    validate_graph(&record.id, &record.graph)?;
    store.policy().check_graph(&record.id, &record.graph)?;
    store.save(&record)?;
    Ok(record)
}

/// A graph an author handed in, resolved from one of the ways they can name it.
///
/// Two ways to say "the graph I mean" — a saved id, or an inline document — so
/// validate, preview, and dry-run all take the same argument whether the author
/// is editing something saved or checking something they have not saved yet.
pub enum GraphHandle<'a> {
    /// A workflow already in the store.
    Saved(&'a str),
    /// A graph document supplied inline.
    Inline(&'a str),
}

impl GraphHandle<'_> {
    /// Resolve to a record, without saving anything.
    pub fn resolve(&self, store: &Arc<dyn WorkflowStore>) -> Result<WorkflowRecord, WorkflowError> {
        match self {
            GraphHandle::Saved(id) => require(store.as_ref(), id),
            GraphHandle::Inline(document) => {
                parse_workflow_with(document, "inline", store.policy())
                    .map_err(WorkflowError::Malformed)
            }
        }
    }
}

/// Validate a graph the author has not necessarily saved.
///
/// Reports every failure, not the first, so one round-trip tells an author
/// everything wrong with what they wrote.
pub fn validate_handle(
    store: &Arc<dyn WorkflowStore>,
    handle: &GraphHandle<'_>,
) -> Result<WorkflowRecord, WorkflowError> {
    let record = handle.resolve(store)?;
    validate_graph(&record.id, &record.graph)?;
    store.policy().check_graph(&record.id, &record.graph)?;
    Ok(record)
}

#[cfg(test)]
#[path = "authoring_tests.rs"]
mod tests;
