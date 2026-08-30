//! Durable observability for the graph runtime — journals, status stores, and
//! the journaling event sink.
//!
//! The live [`crate::graph::stream`] layer emits transient [`GraphEvent`]s into
//! an in-process [`GraphEventSink`]. This module makes that history **durable
//! and correlatable** so a UI, supervisor, or test can reconstruct a recursive
//! graph run tree after the fact:
//!
//! - [`GraphObservation`] — a durable envelope pairing an event with its run
//!   lineage (`run_id` / `parent_run_id` / `root_run_id`), `graph_id`,
//!   `checkpoint_id`, subgraph `namespace`, `step`, `offset`, and timestamp.
//! - [`GraphEventJournal`] — an append-only, offset-addressable journal of
//!   observations, with an [`InMemoryGraphEventJournal`] and a store-backed
//!   [`StoreGraphEventJournal`] (stream key = run id).
//! - [`GraphStatusStore`] — a compact "what is running now?" surface over
//!   [`crate::graph::GraphRunStatus`], with an [`InMemoryGraphStatusStore`].
//! - [`JournalGraphSink`] — a [`GraphEventSink`] that wraps each emitted event
//!   into a [`GraphObservation`] and appends it to a journal, optionally also
//!   forwarding to a live `inner` sink.
//! - [`GraphLatencyMetrics`] / [`GraphHealthSummary`] — rollups derived from a
//!   run's observations: per-step/per-node timings, and per-node
//!   success/failure counts (node-level **tool health** telemetry).
//! - [`GraphLangfuseExporter`] — exports a run's observations to Langfuse,
//!   turning supersteps and nodes into timed spans (failures promoted to
//!   `ERROR`) and attaching the health summary to the trace. It shares the
//!   harness [`LangfuseClient`](crate::harness::observability::LangfuseClient)
//!   transport and defaults its `traceId` to the run's `root_run_id`, so a
//!   graph run and the agent/tool runs its nodes spawn land under one trace.
//!
//! [`CompiledGraph`](crate::graph::CompiledGraph) can be wired to write to a
//! status store and a journal through its builder-style
//! [`with_status_store`](crate::graph::CompiledGraph::with_status_store) and
//! [`with_event_journal`](crate::graph::CompiledGraph::with_event_journal)
//! methods; both are opt-in and default off so existing runs are unchanged.
//!
//! The journaling sink bridges the synchronous [`GraphEventSink::emit`] hook to
//! the async journal API through a background drain (an
//! [`AppendWorker`](crate::harness::observability)): `emit` never blocks the
//! executor on I/O and persistence is best-effort (a full bounded queue drops
//! rather than stalls; backend errors are reported, not propagated). The
//! executor calls [`GraphEventSink::flush`] after the terminal run event so a
//! caller reading the journal right after the run returns sees a complete log.

mod types;

pub use types::*;

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use async_trait::async_trait;

use crate::graph::error::Result;
use crate::graph::ids::{CheckpointId, EventId, GraphId, NodeId, RunId, ThreadId, now_ms};
use crate::graph::status::GraphRunStatus;
use crate::graph::stream::{GraphEvent, GraphEventSink};
use crate::graph::worker::{AppendWorker, DEFAULT_DRAIN_CAPACITY};

// ---------------------------------------------------------------------------
// GraphLatencyMetrics
// ---------------------------------------------------------------------------

impl GraphLatencyMetrics {
    /// Builds latency rollups from durable observations for one graph run.
    ///
    /// Incomplete steps or node executions are ignored because there is no
    /// terminal timestamp to measure against. Duplicate node activations for the
    /// same `(node, step)` are paired in FIFO order.
    pub fn from_observations(observations: &[GraphObservation]) -> Self {
        let mut metrics = Self::default();
        let mut run_start: Option<u64> = None;
        let mut step_starts: HashMap<usize, u64> = HashMap::new();
        let mut node_starts: HashMap<(NodeId, usize), VecDeque<u64>> = HashMap::new();

        for obs in observations {
            match &obs.event {
                GraphEvent::RunStarted { .. } if run_start.is_none() => {
                    run_start = Some(obs.ts_ms);
                }
                GraphEvent::RunStarted { .. } => {}
                GraphEvent::RunCompleted { .. } | GraphEvent::RunFailed { .. } => {
                    if metrics.run_elapsed_ms.is_none()
                        && let Some(start) = run_start
                    {
                        metrics.run_elapsed_ms = Some(obs.ts_ms.saturating_sub(start));
                    }
                }
                GraphEvent::StepStarted { step, .. } => {
                    step_starts.insert(*step, obs.ts_ms);
                }
                GraphEvent::StepCompleted { step } => {
                    if let Some(start) = step_starts.remove(step) {
                        metrics.record_step(GraphStepLatency {
                            step: *step,
                            elapsed_ms: obs.ts_ms.saturating_sub(start),
                        });
                    }
                }
                GraphEvent::NodeStarted { node, step } => {
                    node_starts
                        .entry((node.clone(), *step))
                        .or_default()
                        .push_back(obs.ts_ms);
                }
                GraphEvent::NodeCompleted { node, step } => {
                    if let Some(start) = pop_node_start(&mut node_starts, node, *step) {
                        metrics.record_node(GraphNodeLatency {
                            node: node.clone(),
                            step: *step,
                            elapsed_ms: obs.ts_ms.saturating_sub(start),
                            failed: false,
                        });
                    }
                }
                GraphEvent::NodeFailed { node, step, .. } => {
                    if let Some(start) = pop_node_start(&mut node_starts, node, *step) {
                        metrics.record_node(GraphNodeLatency {
                            node: node.clone(),
                            step: *step,
                            elapsed_ms: obs.ts_ms.saturating_sub(start),
                            failed: true,
                        });
                    }
                }
                _ => {}
            }
        }

        metrics
    }

    /// Builds a run-level latency summary from a compact status snapshot.
    ///
    /// Status snapshots do not contain per-step or per-node timings, but they
    /// do carry started/updated/ended timestamps for end-to-end elapsed time.
    pub fn from_status(status: &GraphRunStatus) -> Self {
        let end = status.ended_at.unwrap_or(status.updated_at);
        Self {
            run_elapsed_ms: duration_ms(status.started_at, end),
            ..Self::default()
        }
    }

    /// Average completed-step latency.
    pub fn average_step_ms(&self) -> Option<u64> {
        average(self.total_step_ms, self.steps.len())
    }

    /// Average completed-node latency.
    pub fn average_node_ms(&self) -> Option<u64> {
        average(self.total_node_ms, self.nodes.len())
    }

    fn record_step(&mut self, latency: GraphStepLatency) {
        self.total_step_ms = self.total_step_ms.saturating_add(latency.elapsed_ms);
        self.max_step_ms = self.max_step_ms.max(latency.elapsed_ms);
        self.steps.push(latency);
    }

    fn record_node(&mut self, latency: GraphNodeLatency) {
        self.total_node_ms = self.total_node_ms.saturating_add(latency.elapsed_ms);
        self.max_node_ms = self.max_node_ms.max(latency.elapsed_ms);
        self.nodes.push(latency);
    }
}

// ---------------------------------------------------------------------------
// GraphHealthSummary
// ---------------------------------------------------------------------------

impl GraphHealthSummary {
    /// Builds a node/tool health rollup from durable observations for one run.
    ///
    /// Counts every `node.started`, `node.completed`, and `node.failed`
    /// observation per node, plus whether the run itself failed. Per-node
    /// entries are sorted by node id so the summary is deterministic.
    pub fn from_observations(observations: &[GraphObservation]) -> Self {
        let mut per_node: HashMap<NodeId, GraphNodeHealth> = HashMap::new();
        let mut summary = Self::default();

        for obs in observations {
            match &obs.event {
                GraphEvent::NodeStarted { node, .. } => {
                    entry_for(&mut per_node, node).started += 1;
                    summary.total_started += 1;
                }
                GraphEvent::NodeCompleted { node, .. } => {
                    entry_for(&mut per_node, node).completed += 1;
                    summary.total_completed += 1;
                }
                GraphEvent::NodeFailed { node, .. } => {
                    entry_for(&mut per_node, node).failed += 1;
                    summary.total_failed += 1;
                }
                GraphEvent::RunFailed { .. } => {
                    summary.run_failed = true;
                }
                _ => {}
            }
        }

        summary.nodes = per_node.into_values().collect();
        summary
            .nodes
            .sort_by(|a, b| a.node.as_str().cmp(b.node.as_str()));
        summary
    }
}

/// Returns the mutable health entry for `node`, inserting a zeroed one keyed by
/// the node id if absent.
fn entry_for<'a>(
    per_node: &'a mut HashMap<NodeId, GraphNodeHealth>,
    node: &NodeId,
) -> &'a mut GraphNodeHealth {
    per_node
        .entry(node.clone())
        .or_insert_with(|| GraphNodeHealth {
            node: node.clone(),
            started: 0,
            completed: 0,
            failed: 0,
        })
}

// ---------------------------------------------------------------------------
// InMemoryGraphEventJournal
// ---------------------------------------------------------------------------

mod in_memory;
use in_memory::{average, duration_ms, pop_node_start};

#[cfg(test)]
#[path = "observability_tests.rs"]
mod test;
