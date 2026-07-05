//! Drives a [`CompiledWorkflow`] to completion by lowering it onto `tinyagents`.
//!
//! `run` builds a fresh `tinyagents` state graph from the validated
//! [`WorkflowGraph`](crate::model::WorkflowGraph) — capturing the run's host
//! [`Capabilities`] in each node handler — then drives it and returns the final
//! run state. State is a [`serde_json::Value`] laid out as
//! `{ "run": { "trigger": … }, "nodes": { "<id>": { "items": [ … ] } } }`;
//! a merge reducer folds each node's item output into that map.
//!
//! Lowering covers the **linear** path (one successor per node), **conditional
//! branching** (successors on distinct ports), **parallel fan-out** (several
//! successors sharing one port, driven by a `Command::goto` that activates every
//! branch concurrently), and a **fan-in barrier** (any node with more than one
//! predecessor is wired with waiting edges so it runs only once all of them
//! finish).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Map, Value, json};
use tinyagents::{
    Command, CompiledGraph, END, GraphBuilder, Interrupt, NodeResult, StateReducer, TinyAgentsError,
};

/// Checkpointer types re-exported from `tinyagents` so a host can name and
/// implement them without taking a direct dependency on `tinyagents`.
///
/// A host that wants durable, cross-process HITL resume implements
/// [`Checkpointer<serde_json::Value>`] (or reuses [`FileCheckpointer`]) and
/// injects it via [`run_with_checkpointer`] / [`resume_with_checkpointer`]. The
/// engine keys persisted state by a caller-supplied `thread_id`.
///
/// [`InMemoryCheckpointer`] is the process-local default used by [`run`],
/// [`run_with_observer`], [`run_resumable`], and [`resume`]; [`DurabilityMode`]
/// configures how aggressively a checkpointer persists.
pub use tinyagents::{Checkpointer, DurabilityMode, FileCheckpointer, InMemoryCheckpointer};

/// Graph-observability types re-exported from `tinyagents` so a host can
/// journal a run's durable [`GraphObservation`]s without taking a direct
/// dependency on `tinyagents`.
///
/// Inject a [`GraphEventJournal`] via [`run_with_checkpointer_journaled`] /
/// [`resume_with_checkpointer_journaled`]; every graph event the run emits is
/// wrapped into a [`GraphObservation`] and appended under the run's
/// `tinyagents` run id (returned on [`JournaledRunOutcome`]), so the host can
/// read the slice back (`journal.read_from(run_id, 0)`) and e.g. export it to
/// Langfuse. [`InMemoryGraphEventJournal`] is a process-local implementation
/// suitable for per-run capture.
pub use tinyagents::{GraphEventJournal, GraphObservation, InMemoryGraphEventJournal};

use crate::caps::Capabilities;
use crate::compiler::CompiledWorkflow;
use crate::data::Item;
use crate::error::{EngineError, Result, ValidationError};
use crate::model::NodeKind;
use crate::nodes::{NodeContext, executor_for};
use crate::observability::{ExecutionStep, Run, RunObserver, RunStatus, StepStatus};

/// Source of process-local run ids. Monotonic and cheap; deliberately not
/// time- or random-based so ids stay deterministic within a process.
static NEXT_RUN_ID: AtomicU64 = AtomicU64::new(0);

/// The result of a completed workflow run.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The final run state after the terminal node(s) completed.
    pub output: Value,
    /// Node ids that paused the run awaiting human approval. A node is listed
    /// here when it is an approval gate (`config.requires_approval == true`)
    /// whose id was not present in the run input's `approvals` array; its
    /// downstream did not run. Empty for a fully completed run.
    pub pending_approvals: Vec<String>,
}

/// The `tinyagents`-minted identifiers of the underlying graph run.
///
/// A [`GraphEventJournal`] attached to a run keys that run's
/// [`GraphObservation`]s by `run_id`, so a host that journaled a run reads the
/// slice back with `journal.read_from(&run_id, 0)`. `root_run_id` is the root
/// of the recursion tree (equal to `run_id` for a top-level run) and is what
/// Langfuse-style exporters default their trace id to.
#[derive(Debug, Clone)]
pub struct GraphRunIds {
    /// The run id of this graph execution — the journal's stream key.
    pub run_id: String,
    /// The root run id of the recursion tree (equals `run_id` at top level).
    pub root_run_id: String,
}

/// The result of a journaled workflow run: the plain [`RunOutcome`] plus the
/// [`GraphRunIds`] needed to read the run's [`GraphObservation`]s back out of
/// the journal the caller injected.
#[derive(Debug, Clone)]
pub struct JournaledRunOutcome {
    /// The workflow-level outcome (final state + pending approval gates).
    pub outcome: RunOutcome,
    /// The `tinyagents` run ids the injected journal keys observations by.
    pub graph_run_ids: GraphRunIds,
}

/// Reducer that deep-merges each node's partial `{ "nodes": { id: { items } } }`
/// update into the run state. Because every node writes under its own id, updates
/// from independent nodes never collide — this stays correct for A2 parallelism.
struct MergeReducer;

impl StateReducer<Value, Value> for MergeReducer {
    fn apply(&self, mut state: Value, update: Value) -> tinyagents::Result<Value> {
        merge(&mut state, update);
        Ok(state)
    }
}

/// Recursively merges `update` into `base`: objects merge key-by-key; any other
/// value (array, scalar, null) overwrites.
fn merge(base: &mut Value, update: Value) {
    match (base, update) {
        (Value::Object(base), Value::Object(update)) => {
            for (key, value) in update {
                merge(base.entry(key).or_insert(Value::Null), value);
            }
        }
        (base, update) => *base = update,
    }
}

/// Collects a node's input items by concatenating the `items` its predecessor
/// nodes emitted into the run state.
fn collect_input(state: &Value, predecessors: &[String]) -> Vec<Item> {
    let mut items = Vec::new();
    for pred in predecessors {
        if let Some(array) = state
            .get("nodes")
            .and_then(|nodes| nodes.get(pred))
            .and_then(|slot| slot.get("items"))
            .and_then(Value::as_array)
        {
            for value in array {
                if let Ok(item) = serde_json::from_value::<Item>(value.clone()) {
                    items.push(item);
                }
            }
        }
    }
    items
}

/// Builds the partial state update a node contributes:
/// `{ "nodes": { id: { items, port? } } }`. The chosen output `port` is recorded
/// only when the node picked one, so conditional edges can route on it.
fn items_update(node_id: &str, items: &[Item], port: Option<&str>) -> tinyagents::Result<Value> {
    let mut slot = Map::new();
    slot.insert("items".to_string(), serde_json::to_value(items)?);
    if let Some(port) = port {
        slot.insert("port".to_string(), Value::String(port.to_string()));
    }
    let mut nodes = Map::new();
    nodes.insert(node_id.to_string(), Value::Object(slot));
    let mut root = Map::new();
    root.insert("nodes".to_string(), Value::Object(nodes));
    Ok(Value::Object(root))
}

/// Builds the error item a node emits when its `on_error` policy is `continue` or
/// `route`, turning a failed execution into routable data rather than a run-ending
/// event: `{ "error": { "message", "node" } }`.
fn error_item(node_id: &str, e: &EngineError) -> Item {
    Item::new(json!({ "error": { "message": e.to_string(), "node": node_id } }))
}

/// Executes a compiled workflow with the given trigger `input` and host
/// `capabilities`, driving it to completion.
///
/// This installs a no-op [`RunObserver`]; use [`run_with_observer`] to receive
/// run/step observability records as the run executes.
///
/// # Errors
/// Returns an [`EngineError`] if lowering, compilation, or execution fails —
/// including any error a node's executor produces. A node kind whose executor is
/// not yet implemented surfaces its `Unimplemented` error here.
pub async fn run(
    workflow: &CompiledWorkflow,
    input: Value,
    capabilities: &Capabilities,
) -> Result<RunOutcome> {
    run_with_observer(
        workflow,
        input,
        capabilities,
        &(Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>),
    )
    .await
}

/// Like [`run`], but reports run/step records to `observer` as the run executes:
/// [`RunObserver::on_run_start`] fires once before any node runs,
/// [`RunObserver::on_step_finish`] once per non-trigger node as it finishes, and
/// [`RunObserver::on_run_finish`] once with the assembled [`Run`]. All execution
/// behavior (retry, `on_error`, HITL interrupts, conditional routing, tracing) is
/// identical to [`run`].
///
/// # Errors
/// Same as [`run`].
pub async fn run_with_observer(
    workflow: &CompiledWorkflow,
    input: Value,
    capabilities: &Capabilities,
    observer: &Arc<dyn RunObserver>,
) -> Result<RunOutcome> {
    // Default (non-injectable) path: a process-local in-memory checkpointer,
    // keyed by the trigger id — identical behavior to before checkpointer
    // injection existed.
    let checkpointer: Arc<dyn Checkpointer<Value>> =
        Arc::new(InMemoryCheckpointer::<Value>::default());
    let thread_id = default_thread_id(workflow)?;
    let (_graph, _thread_id, outcome, _run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        observer,
        checkpointer,
        thread_id,
        None,
    )
    .await?;
    Ok(outcome)
}

/// Builds and compiles the `tinyagents` graph for `workflow`, attaching the
/// host-supplied `checkpointer`, and returns the compiled graph together with
/// the graph's entry (trigger) node id.
///
/// Node handlers capture `observer` (to fire `on_step_finish`) and the shared
/// `steps` sink. This does **not** run the graph — callers either drive it (via
/// `run_with_thread`, see [`build_and_run`]) or resume it from a persisted
/// checkpoint (via `resume`, see [`resume_with_checkpointer`]). Keeping graph
/// construction separate is what lets a host rebuild the identical graph in a
/// fresh process, re-attach the same durable `checkpointer`, and resume.
///
/// When `journal` is supplied the compiled graph is additionally wired with
/// `tinyagents`' durable event journal (see
/// [`CompiledGraph::with_event_journal`]), so every emitted graph event is
/// recorded as a [`GraphObservation`] keyed by the run's `tinyagents` run id.
///
/// # Errors
/// Returns an [`EngineError`] if the workflow has no trigger or if compilation
/// fails.
fn build_graph(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    observer: &Arc<dyn RunObserver>,
    steps: &Arc<Mutex<Vec<ExecutionStep>>>,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    journal: Option<Arc<dyn GraphEventJournal>>,
) -> Result<(CompiledGraph<Value, Value>, String)> {
    let graph = &workflow.graph;

    let trigger = graph
        .trigger()
        .ok_or(EngineError::Validation(ValidationError::MissingTrigger))?;
    let trigger_id = trigger.id.clone();

    // Run-level knobs are read from the trigger node's config — the natural
    // run-level config holder, since `WorkflowGraph` has no top-level config.
    // `recursion_limit` bounds loops (tinyagents' default is 50) and
    // `node_timeout_secs` sets a per-node timeout for the whole run; both are
    // applied to the builder below.
    let recursion_limit = trigger
        .config
        .get("recursion_limit")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);
    let node_timeout_secs = trigger
        .config
        .get("node_timeout_secs")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0);

    tracing::info!(node_count = graph.nodes.len(), trigger = %trigger_id, "workflow run starting");

    // How many predecessors each node has. A node with more than one is a
    // fan-in point: its incoming edges are lowered as waiting edges so it runs
    // only after every predecessor has completed (the merge barrier).
    let mut incoming_counts: std::collections::HashMap<&str, usize> =
        std::collections::HashMap::new();
    for edge in &graph.edges {
        *incoming_counts.entry(edge.to_node.as_str()).or_default() += 1;
    }

    // A node is a **parallel fan-out** point when all of its outgoing edges share
    // a single `from_port` and there is more than one of them: every successor
    // must run concurrently. We record its ordered successor list here so the
    // node's handler can emit a `Command::goto([...])` instead of a plain update.
    let fan_out_targets = |node_id: &str| -> Option<Vec<String>> {
        let outgoing: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.from_node == node_id)
            .collect();
        if outgoing.len() < 2 {
            return None;
        }
        let first_port = outgoing[0].from_port.as_str();
        if outgoing.iter().all(|e| e.from_port == first_port) {
            Some(outgoing.iter().map(|e| e.to_node.clone()).collect())
        } else {
            None
        }
    };

    // Concurrency is required so a fan-out's successors execute in the same
    // superstep; the reducer folds their independent, per-id updates
    // deterministically, so enabling it never changes a linear run's result.
    let mut builder = GraphBuilder::<Value, Value>::new()
        .with_parallel(true)
        .set_reducer(MergeReducer);
    if let Some(limit) = recursion_limit {
        builder = builder.with_recursion_limit(limit as usize);
    }
    if let Some(secs) = node_timeout_secs {
        builder = builder.with_node_timeout(std::time::Duration::from_secs(secs));
    }

    for node in &graph.nodes {
        let node = node.clone();
        let predecessors: Vec<String> = graph
            .edges
            .iter()
            .filter(|e| e.to_node == node.id)
            .map(|e| e.from_node.clone())
            .collect();
        let caps = capabilities.clone();
        let observer = observer.clone();
        let steps = steps.clone();
        let is_trigger = node.kind == NodeKind::Trigger;
        // Successors to fan out to concurrently, or `None` for a non-fan-out node.
        let fan_out = fan_out_targets(&node.id);

        builder = builder.add_node(node.id.clone(), move |state: Value, ctx| {
            let node = node.clone();
            let predecessors = predecessors.clone();
            let caps = caps.clone();
            let observer = observer.clone();
            let steps = steps.clone();
            let fan_out = fan_out.clone();
            // A checkpointed resume (see `ResumableRun::resume`) delivers a resume
            // value to the interrupted node via `NodeContext::resume`; its presence
            // approves this gate so the run proceeds forward from the checkpoint.
            let resumed = ctx.resume.is_some();
            async move {
                // Wraps a node's partial update: a fan-out node drives all of its
                // successors via a `Command::goto`, everything else emits a plain
                // update and follows its static/conditional edge.
                let emit = |update: Value| match &fan_out {
                    Some(targets) => {
                        NodeResult::Command(Command::goto(targets.clone()).with_update(update))
                    }
                    None => NodeResult::Update(update),
                };

                if is_trigger {
                    // The trigger payload is pre-seeded into the state; no-op update
                    // (still fanning out if the trigger has parallel successors).
                    return Ok(emit(json!({})));
                }

                // Human-in-the-loop approval gate. A node whose config sets
                // `requires_approval: true` must not execute until its id is
                // listed in the run input's `approvals` array (readable at
                // `state["run"]["trigger"]["approvals"]`). Until then it pauses
                // the run via a tinyagents interrupt, so its downstream never
                // runs and the run reports the pending node.
                let requires_approval = node
                    .config
                    .get("requires_approval")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if requires_approval {
                    let approved = state
                        .get("run")
                        .and_then(|run| run.get("trigger"))
                        .and_then(|trigger| trigger.get("approvals"))
                        .and_then(Value::as_array)
                        .is_some_and(|approvals| {
                            approvals
                                .iter()
                                .filter_map(Value::as_str)
                                .any(|id| id == node.id)
                        });
                    // `resumed` is set when a checkpointed resume delivered a
                    // resume value to this interrupted gate — that approves it.
                    if !approved && !resumed {
                        tracing::info!(node = %node.id, "node paused awaiting approval");
                        let payload = if node.config.is_null() {
                            json!({})
                        } else {
                            node.config.clone()
                        };
                        return Ok(NodeResult::Interrupt(Interrupt {
                            id: node.id.clone(),
                            node: node.id.clone().into(),
                            payload,
                        }));
                    }
                }

                let input = collect_input(&state, &predecessors);
                let run_meta = state.get("run").cloned().unwrap_or(Value::Null);

                // Per-node error policy, read from free-form `node.config` (no model
                // struct change). `on_error` selects what happens once retries are
                // exhausted; `retry.max_attempts` bounds the attempts.
                let on_error = node
                    .config
                    .get("on_error")
                    .and_then(Value::as_str)
                    .unwrap_or("stop");
                let max_attempts = node
                    .config
                    .get("retry")
                    .and_then(|retry| retry.get("max_attempts"))
                    .and_then(Value::as_u64)
                    .unwrap_or(1)
                    .max(1);
                // Backoff between attempts. `backoff_ms` is the base delay (default
                // 0 = no wait); `backoff` selects `"fixed"` (default, constant delay)
                // or `"exponential"` (`backoff_ms * 2^attempt_index`). We use a
                // runtime-agnostic timer (`futures_timer::Delay`) so the crate stays
                // host-agnostic and pulls in no specific async runtime.
                let backoff_ms = node
                    .config
                    .get("retry")
                    .and_then(|retry| retry.get("backoff_ms"))
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
                let exponential = node
                    .config
                    .get("retry")
                    .and_then(|retry| retry.get("backoff"))
                    .and_then(Value::as_str)
                    == Some("exponential");

                // Attempt the executor up to `max_attempts` times: use the first
                // `Ok`, otherwise keep the last `Err`. Between failed attempts (never
                // after the final one), wait for the configured backoff delay.
                let mut output = None;
                let mut last_err: Option<EngineError> = None;
                let started = Instant::now();
                for attempt in 0..max_attempts {
                    let ctx = NodeContext {
                        node: &node,
                        input: &input,
                        run: &run_meta,
                        caps: &caps,
                    };
                    match executor_for(&node.kind).execute(ctx).await {
                        Ok(ok) => {
                            output = Some(ok);
                            break;
                        }
                        Err(err) => last_err = Some(err),
                    }
                    // Sleep before the next attempt, but not after the last one.
                    if backoff_ms > 0 && attempt + 1 < max_attempts {
                        // `attempt` is the 0-based index of the attempt that just
                        // failed; exponential grows the base by `2^attempt`. All
                        // math saturates and the delay is capped at 60s so a large
                        // `backoff_ms`/attempt count can never overflow or hang.
                        let delay = if exponential {
                            2u64.checked_pow(attempt as u32)
                                .and_then(|factor| backoff_ms.checked_mul(factor))
                                .unwrap_or(u64::MAX)
                        } else {
                            backoff_ms
                        }
                        .min(60_000);
                        futures_timer::Delay::new(std::time::Duration::from_millis(delay)).await;
                    }
                }
                let duration_ms = started.elapsed().as_millis();

                match output {
                    Some(output) => {
                        tracing::debug!(node = %node.id, ?node.kind, item_count = output.items.len(), "node executed");
                        let step = ExecutionStep {
                            node_id: node.id.clone(),
                            status: StepStatus::Success,
                            output: serde_json::to_value(&output.items).unwrap_or(Value::Null),
                            duration_ms,
                        };
                        steps.lock().expect("steps mutex poisoned").push(step.clone());
                        observer.on_step_finish(&step);
                        Ok(emit(items_update(
                            &node.id,
                            &output.items,
                            output.port.as_deref(),
                        )?))
                    }
                    None => {
                        tracing::warn!(node = %node.id, "node failed after retries");
                        let step = ExecutionStep {
                            node_id: node.id.clone(),
                            status: StepStatus::Error,
                            output: Value::Null,
                            duration_ms,
                        };
                        steps.lock().expect("steps mutex poisoned").push(step.clone());
                        observer.on_step_finish(&step);
                        // Retries exhausted. `last_err` is always set when the loop
                        // ran (`max_attempts >= 1`); the `None` arm is unreachable
                        // but handled defensively — emit an empty update, never panic.
                        let Some(err) = last_err else {
                            return Ok(emit(items_update(&node.id, &[], None)?));
                        };
                        match on_error {
                            // Turn the failure into data on the default port.
                            "continue" => Ok(emit(items_update(
                                &node.id,
                                &[error_item(&node.id, &err)],
                                None,
                            )?)),
                            // Turn the failure into data on the `error` port so the
                            // graph can route it to a recovery sub-graph.
                            "route" => Ok(emit(items_update(
                                &node.id,
                                &[error_item(&node.id, &err)],
                                Some("error"),
                            )?)),
                            // "stop" (default) and any unknown policy fail the run.
                            _ => Err(TinyAgentsError::Graph(err.to_string())),
                        }
                    }
                }
            }
        });
    }

    builder = builder.set_entry(trigger_id.clone());
    for node in &graph.nodes {
        // Permit the interrupt at every approval-gate node so the engine can
        // pause there (the gate emits the interrupt from its handler above).
        if node
            .config
            .get("requires_approval")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            builder = builder.mark_interrupt(node.id.clone());
        }
        let outgoing: Vec<_> = graph
            .edges
            .iter()
            .filter(|e| e.from_node == node.id)
            .collect();
        if outgoing.is_empty() {
            // Leaf node: nothing routes out, so it terminates the run.
            builder = builder.add_edge(node.id.clone(), END);
        } else if let Some(dests) = fan_out_targets(&node.id) {
            // Parallel fan-out: the node's handler drives every successor with a
            // `Command::goto`, so we only declare the destination hints here. A
            // command-routing node may not also carry static/conditional edges,
            // so nothing else is wired for it.
            builder = builder.with_command_destinations(node.id.clone(), dests);
        } else if let [edge] = outgoing.as_slice() {
            // Single successor. If the target is a fan-in point (more than one
            // predecessor, e.g. a `merge`) wire it as a waiting edge so it runs
            // only once every predecessor has completed — the merge barrier.
            // Otherwise a plain static edge.
            let target = edge.to_node.clone();
            if incoming_counts
                .get(edge.to_node.as_str())
                .copied()
                .unwrap_or(0)
                > 1
            {
                builder = builder.add_waiting_edge(node.id.clone(), target);
            } else {
                builder = builder.add_edge(node.id.clone(), target);
            }
        } else {
            // Branching: distinct ports lower to conditional edges keyed on the
            // port the node recorded into state (defaulting to `main`).
            let from = node.id.clone();
            let routes: Vec<(String, String)> = outgoing
                .iter()
                .map(|e| (e.from_port.clone(), e.to_node.clone()))
                .collect();
            builder = builder.add_conditional_edges(
                node.id.clone(),
                move |state: &Value| -> String {
                    state
                        .get("nodes")
                        .and_then(|nodes| nodes.get(&from))
                        .and_then(|slot| slot.get("port"))
                        .and_then(Value::as_str)
                        .unwrap_or("main")
                        .to_string()
                },
                routes,
            );
        }
    }

    // A checkpointer (plus a thread id on the run) is required for tinyagents to
    // persist the interrupt boundary and hand pending approvals back to us. The
    // checkpointer is host-injected: the default entry points supply an
    // in-memory one to keep the crate host-agnostic and dep-free, while a host
    // can inject a durable store for cross-process resume.
    let mut compiled = builder
        .compile()
        .map_err(|e| EngineError::Capability(e.to_string()))?
        .with_checkpointer(checkpointer);

    // Opt-in durable observability: with a journal attached, tinyagents wraps
    // every emitted graph event into a `GraphObservation` (stamped with run
    // lineage + step) and appends it under the run id.
    if let Some(journal) = journal {
        tracing::debug!("attaching graph event journal to compiled workflow graph");
        compiled = compiled.with_event_journal(journal);
    }

    Ok((compiled, trigger_id))
}

/// The default thread id for a run: the workflow's trigger (entry) node id.
///
/// Used by the non-injectable entry points ([`run`], [`run_with_observer`],
/// [`run_resumable`]) so a run is keyed under a stable, workflow-derived id —
/// preserving the pre-injectable-checkpointer behavior exactly.
///
/// # Errors
/// Returns [`EngineError::Validation`] if the workflow has no trigger node.
fn default_thread_id(workflow: &CompiledWorkflow) -> Result<String> {
    Ok(workflow
        .graph
        .trigger()
        .ok_or(EngineError::Validation(ValidationError::MissingTrigger))?
        .id
        .clone())
}

/// Builds the `tinyagents` graph for `workflow` under the supplied
/// `checkpointer`, drives the first run keyed under `thread_id`, and returns the
/// still-live compiled graph, that `thread_id`, and the [`RunOutcome`].
///
/// Shared by [`run_with_observer`] / [`run_with_checkpointer`] — which discard
/// the graph — and [`run_resumable`], which keeps it (and thus its checkpointer)
/// alive so a later [`ResumableRun::resume`] can replay forward from the
/// persisted checkpoint without re-executing already-completed nodes.
///
/// # Errors
/// Same as [`run`].
async fn build_and_run(
    workflow: &CompiledWorkflow,
    input: Value,
    capabilities: &Capabilities,
    observer: &Arc<dyn RunObserver>,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: String,
    journal: Option<Arc<dyn GraphEventJournal>>,
) -> Result<(CompiledGraph<Value, Value>, String, RunOutcome, GraphRunIds)> {
    // Process-local, monotonic run id — no time/random source.
    let run_id = format!("run-{}", NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed));
    observer.on_run_start(&run_id);

    // Node handlers stream finished steps here (they also fire
    // `on_step_finish`); the engine folds this into the final `Run`. A shared
    // `Mutex` is the simplest correct sink across the `'static + Send + Sync`
    // handler closures, which can't otherwise push to a common `Vec`.
    let steps: Arc<Mutex<Vec<ExecutionStep>>> = Arc::new(Mutex::new(Vec::new()));

    // Build and compile the graph under the host-supplied checkpointer;
    // `trigger_id` is the graph's entry node.
    let (compiled, trigger_id) = build_graph(
        workflow,
        capabilities,
        observer,
        &steps,
        checkpointer,
        journal,
    )?;

    let seed_items = items_update(&trigger_id, &[Item::new(input.clone())], None)
        .map_err(|e| EngineError::Capability(e.to_string()))?;
    let mut initial = json!({ "run": { "trigger": input } });
    merge(&mut initial, seed_items);

    // The run is keyed under the caller-supplied `thread_id` (the default paths
    // pass the trigger id, preserving prior behavior); this is where the
    // checkpointer persists the interrupt boundary.
    let execution = compiled
        .run_with_thread(thread_id.clone(), initial)
        .await
        .map_err(|e| EngineError::Capability(e.to_string()))?;

    // Nodes that paused the run awaiting approval, surfaced from the interrupts
    // tinyagents returned at the boundary.
    let pending_approvals: Vec<String> = execution
        .interrupts
        .iter()
        .map(|interrupt| interrupt.node.as_str().to_string())
        .collect();

    tracing::info!(
        steps = execution.steps,
        visited = execution.visited.len(),
        pending_approvals = pending_approvals.len(),
        "workflow run finished"
    );

    // Reaching here means the run settled without a `stop`-policy failure
    // (those bubble out as `Err` above), so it Completed. Per-step Error status
    // is recorded independently on nodes handled by `continue`/`route`.
    let run_record = Run {
        id: run_id,
        status: RunStatus::Completed,
        steps: steps.lock().expect("steps mutex poisoned").clone(),
    };
    observer.on_run_finish(&run_record);

    // The tinyagents-minted run ids: a journal (when attached) keys this run's
    // observations by `run_id`, so surface both ids to the caller.
    let graph_run_ids = GraphRunIds {
        run_id: execution.run_id.as_str().to_string(),
        root_run_id: execution.root_run_id.as_str().to_string(),
    };

    Ok((
        compiled,
        thread_id,
        RunOutcome {
            output: execution.state,
            pending_approvals,
        },
        graph_run_ids,
    ))
}

/// Resumes a paused run by re-running `workflow` with `newly_approved` node ids
/// added to the run's approvals, so previously-gated nodes now execute.
///
/// This is the approve-and-continue completion of the human-in-the-loop loop:
/// a run that paused with `RunOutcome::pending_approvals`
/// is continued by supplying the approvals. It re-executes the workflow with the
/// merged approval set (deterministic; checkpointed super-step replay is a future
/// optimization). Prior approvals in `input["approvals"]` are preserved and unioned.
///
/// # Errors
/// Same as [`run`]: returns an [`EngineError`] if lowering, compilation, or
/// execution of the resumed run fails.
pub async fn resume(
    workflow: &CompiledWorkflow,
    input: Value,
    newly_approved: Vec<String>,
    capabilities: &Capabilities,
) -> Result<RunOutcome> {
    // Union `newly_approved` into `input["approvals"]`: start from any existing
    // approvals array (ignoring non-string entries), then append each newly
    // approved id that is not already present. Reading defensively — a missing or
    // non-array `approvals` simply yields an empty starting set, never a panic.
    let mut approvals: Vec<String> = input
        .get("approvals")
        .and_then(Value::as_array)
        .map(|existing| {
            existing
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    for id in newly_approved {
        if !approvals.contains(&id) {
            approvals.push(id);
        }
    }

    let mut merged_input = input;
    if let Value::Object(map) = &mut merged_input {
        map.insert("approvals".to_string(), json!(approvals));
    } else {
        // A non-object input carries no fields to preserve, so replace it with a
        // fresh object holding just the merged approvals.
        merged_input = json!({ "approvals": approvals });
    }

    run(workflow, merged_input, capabilities).await
}

/// A live, resumable workflow run.
///
/// Unlike the re-run-based [`resume`], this keeps the compiled `tinyagents` graph
/// (and therefore its checkpointer) alive after the initial run, so
/// [`ResumableRun::resume`] can continue **from the persisted checkpoint** —
/// tinyagents replays forward from the interrupt boundary, so nodes that already
/// completed are **not** re-executed.
pub struct ResumableRun {
    /// The compiled graph that ran, kept alive so its in-memory checkpointer
    /// still holds the interrupt boundary a resume replays from.
    graph: CompiledGraph<Value, Value>,
    /// The thread id the initial run (and every resume) is keyed under.
    thread_id: String,
    /// The outcome of the initial run, before any resume.
    outcome: RunOutcome,
}

impl ResumableRun {
    /// The outcome of the initial run, before any [`resume`](ResumableRun::resume).
    /// Its [`RunOutcome::pending_approvals`] lists the gate nodes awaiting
    /// approval.
    pub fn outcome(&self) -> &RunOutcome {
        &self.outcome
    }

    /// Resumes the run from its checkpoint, approving the currently-interrupted
    /// gate node(s) so the workflow proceeds. `newly_approved` are the gate ids
    /// being approved; they are also recorded into the run's approvals for
    /// downstream visibility.
    ///
    /// tinyagents replays forward from the persisted checkpoint — the interrupted
    /// gate re-runs (now approved, because the resume value reaches it via
    /// `NodeContext::resume`) and its downstream continues, while nodes that
    /// already completed are not re-executed.
    ///
    /// # Errors
    /// Returns [`EngineError::Capability`] if the checkpointed resume fails (for
    /// example, when there is no pending checkpoint to resume from).
    pub async fn resume(&self, newly_approved: Vec<String>) -> Result<RunOutcome> {
        let approvals_update = json!({
            "run": { "trigger": { "approvals": newly_approved } }
        });
        let execution = self
            .graph
            .resume(
                self.thread_id.as_str(),
                Command::resume(json!(true)).with_update(approvals_update),
            )
            .await
            .map_err(|e| EngineError::Capability(e.to_string()))?;

        let pending_approvals: Vec<String> = execution
            .interrupts
            .iter()
            .map(|interrupt| interrupt.node.as_str().to_string())
            .collect();

        Ok(RunOutcome {
            output: execution.state,
            pending_approvals,
        })
    }
}

/// Runs `workflow` like [`run`], but returns a [`ResumableRun`] whose compiled
/// graph (and checkpointer) is kept alive so [`ResumableRun::resume`] can
/// continue from the persisted checkpoint without re-executing completed nodes.
///
/// A no-op [`RunObserver`] is installed; all execution behavior is identical to
/// [`run`].
///
/// # Errors
/// Same as [`run`].
pub async fn run_resumable(
    workflow: &CompiledWorkflow,
    input: Value,
    capabilities: &Capabilities,
) -> Result<ResumableRun> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    // Default (non-injectable) path: a process-local in-memory checkpointer,
    // kept alive on the returned `ResumableRun`, keyed by the trigger id.
    let checkpointer: Arc<dyn Checkpointer<Value>> =
        Arc::new(InMemoryCheckpointer::<Value>::default());
    let thread_id = default_thread_id(workflow)?;
    let (graph, thread_id, outcome, _run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        &observer,
        checkpointer,
        thread_id,
        None,
    )
    .await?;
    Ok(ResumableRun {
        graph,
        thread_id,
        outcome,
    })
}

/// Runs `workflow` under a **host-injected** `checkpointer`, keying the run's
/// persisted state by the caller-supplied `thread_id`.
///
/// This is the durable, cross-process entry point. Unlike [`run`] — which uses a
/// process-local [`InMemoryCheckpointer`] keyed by the trigger id — this drives
/// the run under whatever [`Checkpointer`] the host supplies (for example a
/// database-backed run ledger), keyed by a stable `thread_id` the host chooses.
/// When the run pauses at a human-in-the-loop approval gate, its interrupt
/// boundary is persisted into the host's checkpointer under `thread_id`; the
/// returned [`RunOutcome::pending_approvals`] lists the gate node ids awaiting
/// approval, and their downstream did not run.
///
/// A host can then continue the run later — even after a process restart — by
/// rebuilding its [`Capabilities`] and the same checkpointer and calling
/// [`resume_with_checkpointer`] with the same `thread_id`.
///
/// A no-op [`RunObserver`] is installed; all execution behavior (retry,
/// `on_error`, HITL interrupts, conditional routing, tracing) is identical to
/// [`run`].
///
/// # Errors
/// Same as [`run`]: returns an [`EngineError`] if lowering, compilation, or
/// execution (including any node executor error) fails.
pub async fn run_with_checkpointer(
    workflow: &CompiledWorkflow,
    input: Value,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
) -> Result<RunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    let (_graph, _thread_id, outcome, _run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        &observer,
        checkpointer,
        thread_id.to_string(),
        None,
    )
    .await?;
    Ok(outcome)
}

/// Resumes a run that was previously started with [`run_with_checkpointer`],
/// continuing it **from the persisted checkpoint** in the host-injected
/// `checkpointer`.
///
/// This is the durable, cross-process resume path. It rebuilds the identical
/// `tinyagents` graph for `workflow`, re-attaches the **same** `checkpointer`,
/// and resumes the persisted `thread_id` — so a host can run, persist to its
/// own durable store, and later (even after a full process restart) reconstruct
/// its [`Capabilities`] plus checkpointer and pick the run back up by
/// `thread_id`. Nodes that already completed before the pause are not
/// re-executed; tinyagents replays forward from the interrupt boundary.
///
/// `newly_approved` are the gate node ids being approved. Approval flows through
/// the same mechanism [`ResumableRun::resume`] uses: [`Command::resume`]
/// delivers a resume value that reaches the interrupted gate via
/// `NodeContext::resume`, which the gate treats as approval. The ids are also
/// recorded into the run's approvals for downstream visibility. (Note: in
/// tinyagents the accompanying state update is ignored on resume, so the resume
/// value itself is the operative approval channel.)
///
/// Returns a fresh [`RunOutcome`]: `output` is the resumed run's final state and
/// `pending_approvals` lists any gate still awaiting approval (empty once the
/// run completes).
///
/// # Errors
/// Returns [`EngineError`] if rebuilding/compiling the graph fails, or
/// [`EngineError::Capability`] if the checkpointed resume fails — for example
/// when the `checkpointer` holds no pending checkpoint for `thread_id`.
pub async fn resume_with_checkpointer(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    newly_approved: Vec<String>,
) -> Result<RunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    let (outcome, _run_ids) = resume_with_checkpointer_inner(
        workflow,
        capabilities,
        checkpointer,
        thread_id,
        newly_approved,
        None,
        &observer,
    )
    .await?;
    Ok(outcome)
}

/// Like [`run_with_checkpointer`], but additionally attaches the host-supplied
/// `journal`: every graph event the run emits is recorded as a durable
/// [`GraphObservation`] keyed by the run's `tinyagents` run id, which is
/// returned on the [`JournaledRunOutcome`] so the host can read the exact
/// slice back (`journal.read_from(&graph_run_ids.run_id, 0)`) — for example to
/// export the run to Langfuse after it settles.
///
/// All execution behavior is identical to [`run_with_checkpointer`]; the
/// journal sits off the hot path (appends are best-effort inside `tinyagents`)
/// and never fails the run.
///
/// # Errors
/// Same as [`run_with_checkpointer`].
pub async fn run_with_checkpointer_journaled(
    workflow: &CompiledWorkflow,
    input: Value,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    journal: Arc<dyn GraphEventJournal>,
) -> Result<JournaledRunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    run_with_checkpointer_journaled_observed(
        workflow,
        input,
        capabilities,
        checkpointer,
        thread_id,
        journal,
        &observer,
    )
    .await
}

/// Like [`run_with_checkpointer_journaled`], but additionally reports live
/// run/step records to the host-supplied `observer` as the run executes
/// ([`RunObserver::on_run_start`] once, [`RunObserver::on_step_finish`] per
/// non-trigger node as it finishes, [`RunObserver::on_run_finish`] once at
/// settle). This is the durable + journaled + observed entry point a host uses
/// when it wants **both** post-run journal export **and** live per-step
/// observation (e.g. incremental run-history persistence and a progress feed).
///
/// The observer is held as `Arc<dyn RunObserver>` and cloned into each node
/// handler, which run across threads, so it must be cheap and non-blocking; see
/// [`RunObserver`]'s contract.
///
/// # Errors
/// Same as [`run_with_checkpointer_journaled`].
pub async fn run_with_checkpointer_journaled_observed(
    workflow: &CompiledWorkflow,
    input: Value,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    journal: Arc<dyn GraphEventJournal>,
    observer: &Arc<dyn RunObserver>,
) -> Result<JournaledRunOutcome> {
    let (_graph, _thread_id, outcome, graph_run_ids) = build_and_run(
        workflow,
        input,
        capabilities,
        observer,
        checkpointer,
        thread_id.to_string(),
        Some(journal),
    )
    .await?;
    tracing::debug!(
        run_id = %graph_run_ids.run_id,
        root_run_id = %graph_run_ids.root_run_id,
        "journaled workflow run finished"
    );
    Ok(JournaledRunOutcome {
        outcome,
        graph_run_ids,
    })
}

/// Like [`resume_with_checkpointer`], but additionally attaches the
/// host-supplied `journal` to the resumed run (see
/// [`run_with_checkpointer_journaled`] for the journaling contract). The
/// resumed execution mints a **new** `tinyagents` run id — returned on the
/// [`JournaledRunOutcome`] — so the host reads the resume's observations under
/// that id, not the original run's.
///
/// # Errors
/// Same as [`resume_with_checkpointer`].
pub async fn resume_with_checkpointer_journaled(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    newly_approved: Vec<String>,
    journal: Arc<dyn GraphEventJournal>,
) -> Result<JournaledRunOutcome> {
    let observer = Arc::new(crate::observability::NoopObserver) as Arc<dyn RunObserver>;
    resume_with_checkpointer_journaled_observed(
        workflow,
        capabilities,
        checkpointer,
        thread_id,
        newly_approved,
        journal,
        &observer,
    )
    .await
}

/// Like [`resume_with_checkpointer_journaled`], but additionally reports live
/// step records to the host-supplied `observer` as the resumed run executes
/// (each node that runs after the interrupt boundary fires
/// [`RunObserver::on_step_finish`]). The durable + journaled + observed resume
/// counterpart to [`run_with_checkpointer_journaled_observed`].
///
/// # Errors
/// Same as [`resume_with_checkpointer_journaled`].
pub async fn resume_with_checkpointer_journaled_observed(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    newly_approved: Vec<String>,
    journal: Arc<dyn GraphEventJournal>,
    observer: &Arc<dyn RunObserver>,
) -> Result<JournaledRunOutcome> {
    let (outcome, graph_run_ids) = resume_with_checkpointer_inner(
        workflow,
        capabilities,
        checkpointer,
        thread_id,
        newly_approved,
        Some(journal),
        observer,
    )
    .await?;
    tracing::debug!(
        run_id = %graph_run_ids.run_id,
        root_run_id = %graph_run_ids.root_run_id,
        "journaled workflow resume finished"
    );
    Ok(JournaledRunOutcome {
        outcome,
        graph_run_ids,
    })
}

/// Shared implementation of the checkpointed resume path: rebuilds the graph
/// (optionally journaled), re-attaches the same `checkpointer`, and resumes
/// `thread_id`. Returns the outcome plus the resumed execution's
/// `tinyagents`-minted run ids.
async fn resume_with_checkpointer_inner(
    workflow: &CompiledWorkflow,
    capabilities: &Capabilities,
    checkpointer: Arc<dyn Checkpointer<Value>>,
    thread_id: &str,
    newly_approved: Vec<String>,
    journal: Option<Arc<dyn GraphEventJournal>>,
    observer: &Arc<dyn RunObserver>,
) -> Result<(RunOutcome, GraphRunIds)> {
    let steps: Arc<Mutex<Vec<ExecutionStep>>> = Arc::new(Mutex::new(Vec::new()));

    // Rebuild the identical graph and re-attach the SAME checkpointer, so
    // `resume` loads the state persisted under `thread_id`. Node handlers fire
    // `observer.on_step_finish` for every node that runs after the interrupt
    // boundary, so a host observer sees the resumed steps live.
    let (compiled, _trigger_id) = build_graph(
        workflow,
        capabilities,
        observer,
        &steps,
        checkpointer,
        journal,
    )?;

    // Approvals recorded for downstream visibility. On resume the interrupted
    // gate is approved because the resume value reaches it via
    // `NodeContext::resume`; the `with_update` mirrors `ResumableRun::resume`
    // (tinyagents ignores it on resume, so the resume value is the real
    // approval channel).
    let approvals_update = json!({
        "run": { "trigger": { "approvals": newly_approved } }
    });
    let execution = compiled
        .resume(
            thread_id,
            Command::resume(json!(true)).with_update(approvals_update),
        )
        .await
        .map_err(|e| EngineError::Capability(e.to_string()))?;

    let pending_approvals: Vec<String> = execution
        .interrupts
        .iter()
        .map(|interrupt| interrupt.node.as_str().to_string())
        .collect();

    let graph_run_ids = GraphRunIds {
        run_id: execution.run_id.as_str().to_string(),
        root_run_id: execution.root_run_id.as_str().to_string(),
    };

    Ok((
        RunOutcome {
            output: execution.state,
            pending_approvals,
        },
        graph_run_ids,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::mock::mock_capabilities;
    use crate::compiler::compile;
    use crate::model::{Edge, Node, WorkflowGraph};
    use std::sync::{Arc, Mutex};

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.to_string(),
            kind,
            type_version: 1,
            name: id.to_string(),
            config: Value::Null,
            ports: Vec::new(),
            position: None,
        }
    }

    #[tokio::test]
    async fn trigger_only_workflow_runs_end_to_end() {
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger)],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "hello": "world" }), &caps)
            .await
            .expect("run");

        assert_eq!(
            outcome.output["run"]["trigger"],
            json!({ "hello": "world" })
        );
        assert_eq!(
            outcome.output["nodes"]["t"]["items"][0]["json"],
            json!({ "hello": "world" })
        );
    }

    #[tokio::test]
    async fn linear_edge_drives_downstream_node() {
        // trigger -> output_parser (a passthrough): proves edge lowering + dispatch
        // by checking the trigger items flow through to the downstream node.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("p", NodeKind::OutputParser),
            ],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "p".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["p"]["items"][0]["json"],
            json!({ "x": 1 })
        );
    }

    #[tokio::test]
    async fn journaled_run_records_graph_observations() {
        // trigger -> output_parser under run_with_checkpointer_journaled: the
        // injected in-memory journal must hold this run's durable
        // GraphObservations (node started/completed) under the tinyagents run
        // id returned on the JournaledRunOutcome.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("p", NodeKind::OutputParser),
            ],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "p".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();
        let checkpointer: Arc<dyn Checkpointer<Value>> =
            Arc::new(InMemoryCheckpointer::<Value>::default());
        let journal = Arc::new(InMemoryGraphEventJournal::new());

        let journaled = run_with_checkpointer_journaled(
            &compiled,
            json!({ "x": 1 }),
            &caps,
            checkpointer,
            "thread-journal-1",
            journal.clone(),
        )
        .await
        .expect("journaled run");

        // The workflow outcome is unchanged from the plain checkpointed path.
        assert_eq!(
            journaled.outcome.output["nodes"]["p"]["items"][0]["json"],
            json!({ "x": 1 })
        );
        assert!(journaled.outcome.pending_approvals.is_empty());

        // The returned run id is the journal's stream key: reading it back
        // replays the run's durable observations.
        let run_id = &journaled.graph_run_ids.run_id;
        assert!(!run_id.is_empty(), "run id must be surfaced");
        assert_eq!(
            journaled.graph_run_ids.root_run_id, *run_id,
            "top-level run: root run id equals run id"
        );
        let observations = journal.read_from(run_id, 0).await.expect("read journal");
        assert!(
            !observations.is_empty(),
            "journal must hold observations for run {run_id}"
        );

        let kinds: Vec<&str> = observations.iter().map(|o| o.event.kind()).collect();
        // Both graph nodes ran: their handler start/completion events are
        // journaled, alongside the run lifecycle.
        assert!(kinds.contains(&"run.started"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"node.started"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"node.completed"), "kinds: {kinds:?}");
        assert!(kinds.contains(&"run.completed"), "kinds: {kinds:?}");
        // Every observation is keyed by the surfaced run id and stamped with
        // the caller's thread id.
        for obs in &observations {
            assert_eq!(obs.run_id.as_str(), run_id);
            assert_eq!(
                obs.thread_id.as_ref().map(|t| t.as_str()),
                Some("thread-journal-1")
            );
        }
    }

    #[tokio::test]
    async fn condition_routes_only_the_taken_branch() {
        // trigger -> condition(field=active) branches to pass_a (true) / pass_b
        // (false), both passthroughs. A truthy input must run only the true branch.
        let mut condition = node("c", NodeKind::Condition);
        condition.config = json!({ "field": "active" });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                condition,
                node("pass_a", NodeKind::OutputParser),
                node("pass_b", NodeKind::OutputParser),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "c".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "c".to_string(),
                    from_port: "true".to_string(),
                    to_node: "pass_a".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "c".to_string(),
                    from_port: "false".to_string(),
                    to_node: "pass_b".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "active": true }), &caps)
            .await
            .expect("run");
        assert!(
            !outcome.output["nodes"]["pass_a"]["items"].is_null(),
            "true branch should have run"
        );
        assert!(
            outcome.output["nodes"]["pass_b"].is_null(),
            "false branch should not have run"
        );
    }

    #[tokio::test]
    async fn fan_out_runs_both_branches() {
        // trigger fans out on port `main` to two independent successors; both must
        // run concurrently (previously this shape was rejected as unimplemented).
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("a", NodeKind::Transform),
                node("b", NodeKind::Transform),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "a".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "b".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");
        assert!(
            !outcome.output["nodes"]["a"]["items"].is_null(),
            "fan-out branch a should have run"
        );
        assert!(
            !outcome.output["nodes"]["b"]["items"].is_null(),
            "fan-out branch b should have run"
        );
    }

    #[tokio::test]
    async fn diamond_fan_out_and_merge() {
        // trigger -> dispatch, which fans out on port `main` to `a` and `b`; both
        // feed a `merge` barrier `m`, then `m -> done`. The barrier must hold until
        // both branches complete, and merge concatenates their items.
        let edge = |from: &str, port: &str, to: &str| Edge {
            from_node: from.to_string(),
            from_port: port.to_string(),
            to_node: to.to_string(),
            to_port: "main".to_string(),
        };
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("d", NodeKind::OutputParser),
                node("a", NodeKind::OutputParser),
                node("b", NodeKind::OutputParser),
                node("m", NodeKind::Merge),
                node("done", NodeKind::OutputParser),
            ],
            edges: vec![
                edge("t", "main", "d"),
                edge("d", "main", "a"),
                edge("d", "main", "b"),
                edge("a", "main", "m"),
                edge("b", "main", "m"),
                edge("m", "main", "done"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");

        assert!(
            !outcome.output["nodes"]["a"]["items"].is_null(),
            "fan-out branch a should have run"
        );
        assert!(
            !outcome.output["nodes"]["b"]["items"].is_null(),
            "fan-out branch b should have run"
        );
        let merged = outcome.output["nodes"]["m"]["items"]
            .as_array()
            .expect("merge should have produced items");
        assert!(
            merged.len() >= 2,
            "merge should concatenate both branches' items, got {}",
            merged.len()
        );
        assert!(
            !outcome.output["nodes"]["done"]["items"].is_null(),
            "the node past the merge barrier should have run"
        );
    }

    #[tokio::test]
    async fn on_error_continue_emits_error_item() {
        // A `tool_call` with no `slug` deterministically errors; `on_error:
        // continue` turns that into an error item on the default port so the run
        // still completes.
        let mut tool = node("x", NodeKind::ToolCall);
        tool.config = json!({ "on_error": "continue" });
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), tool],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "x".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({}), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["x"]["items"][0]["json"]["error"]["node"],
            json!("x")
        );
    }

    #[tokio::test]
    async fn on_error_route_sends_error_item_to_error_port() {
        // `on_error: route` emits the error item on the `error` port; an edge from
        // that port must carry it into the downstream handler.
        let mut tool = node("x", NodeKind::ToolCall);
        tool.config = json!({ "on_error": "route" });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                tool,
                node("h", NodeKind::OutputParser),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "x".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "x".to_string(),
                    from_port: "error".to_string(),
                    to_node: "h".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({}), &caps).await.expect("run");
        assert!(
            !outcome.output["nodes"]["h"]["items"][0]["json"]["error"].is_null(),
            "handler should have received the routed error item"
        );
    }

    #[tokio::test]
    async fn on_error_stop_is_default() {
        // No `on_error` config: the tool_call's error must fail the whole run.
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("x", NodeKind::ToolCall)],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "x".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        assert!(run(&compiled, json!({}), &caps).await.is_err());
    }

    #[tokio::test]
    async fn retry_then_continue_completes() {
        // Retries are exhausted (the tool_call errors every attempt), then
        // `on_error: continue` yields an error item and the run completes.
        let mut tool = node("x", NodeKind::ToolCall);
        tool.config = json!({ "retry": { "max_attempts": 3 }, "on_error": "continue" });
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), tool],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "x".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({}), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["x"]["items"][0]["json"]["error"]["node"],
            json!("x")
        );
    }

    #[tokio::test]
    async fn retry_backoff_runs_without_hanging() {
        // trigger -> tool_call with no slug (deterministic error) and an
        // exponential backoff of 1ms across 2 attempts. The tiny delay proves the
        // backoff path executes between attempts without hanging, and `on_error:
        // continue` lets the run complete with an error item. (Actual timeout/limit
        // firing is enforced and tested by tinyagents itself.)
        let mut tool = node("x", NodeKind::ToolCall);
        tool.config = json!({
            "retry": { "max_attempts": 2, "backoff_ms": 1, "backoff": "exponential" },
            "on_error": "continue"
        });
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), tool],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "x".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({}), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["x"]["items"][0]["json"]["error"]["node"],
            json!("x")
        );
    }

    #[tokio::test]
    async fn run_level_knobs_accepted() {
        // A trigger carrying run-level `recursion_limit` and `node_timeout_secs`
        // wired to a downstream passthrough. This proves the knobs are read from the
        // trigger config and wired onto the builder without breaking execution; the
        // downstream node still runs. (tinyagents itself tests the knobs actually
        // firing.)
        let mut trigger = node("t", NodeKind::Trigger);
        trigger.config = json!({ "recursion_limit": 100, "node_timeout_secs": 30 });
        let graph = WorkflowGraph {
            nodes: vec![trigger, node("p", NodeKind::OutputParser)],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "p".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["p"]["items"][0]["json"],
            json!({ "x": 1 })
        );
    }

    #[tokio::test]
    async fn run_is_instrumented_and_still_succeeds() {
        // Regression guard: the `tracing` instrumentation added to `run` must not
        // alter execution. Drive a simple `trigger -> output_parser` workflow and
        // confirm the items still flow through with the instrumentation present.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("p", NodeKind::OutputParser),
            ],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "p".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "ok": true }), &caps)
            .await
            .expect("instrumented run should still succeed");
        assert_eq!(
            outcome.output["nodes"]["p"]["items"][0]["json"],
            json!({ "ok": true })
        );
    }

    #[tokio::test]
    async fn approval_gate_pauses_until_approved() {
        // trigger -> gate{requires_approval} -> downstream. With no approvals in
        // the input the gate must pause the run: it reports as pending and its
        // downstream never runs.
        let mut gate = node("gate", NodeKind::OutputParser);
        gate.config = json!({ "requires_approval": true });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate,
                node("downstream", NodeKind::OutputParser),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "gate".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "main".to_string(),
                    to_node: "downstream".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
        assert!(
            outcome.pending_approvals.contains(&"gate".to_string()),
            "gate should be reported as pending approval"
        );
        assert!(
            outcome.output["nodes"]["downstream"].is_null(),
            "downstream must not run while the gate is pending"
        );
    }

    #[tokio::test]
    async fn approved_gate_completes() {
        // Same graph, but the input approves the gate: the run completes fully
        // and the downstream node runs.
        let mut gate = node("gate", NodeKind::OutputParser);
        gate.config = json!({ "requires_approval": true });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate,
                node("downstream", NodeKind::OutputParser),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "gate".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "main".to_string(),
                    to_node: "downstream".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "approvals": ["gate"] }), &caps)
            .await
            .expect("run");
        assert!(
            outcome.pending_approvals.is_empty(),
            "no approvals should be pending once the gate is approved"
        );
        assert!(
            !outcome.output["nodes"]["downstream"]["items"].is_null(),
            "downstream should run once the gate is approved"
        );
    }

    /// A [`RunObserver`] that records which node ids finished and how many runs
    /// started, so a test can assert the observer hooks fired.
    struct Capture {
        steps: Arc<Mutex<Vec<String>>>,
        runs: Arc<Mutex<u32>>,
    }

    impl RunObserver for Capture {
        fn on_run_start(&self, _run_id: &str) {
            *self.runs.lock().unwrap() += 1;
        }

        fn on_step_finish(&self, step: &ExecutionStep) {
            self.steps.lock().unwrap().push(step.node_id.clone());
        }
    }

    #[tokio::test]
    async fn observer_receives_run_start_and_step_finish() {
        // trigger -> output_parser via `run_with_observer`: on_run_start fires
        // once and on_step_finish records the (non-trigger) output_parser node.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("p", NodeKind::OutputParser),
            ],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "p".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let steps = Arc::new(Mutex::new(Vec::new()));
        let runs = Arc::new(Mutex::new(0));
        let observer: Arc<dyn RunObserver> = Arc::new(Capture {
            steps: steps.clone(),
            runs: runs.clone(),
        });

        run_with_observer(&compiled, json!({ "x": 1 }), &caps, &observer)
            .await
            .expect("run");

        assert_eq!(*runs.lock().unwrap(), 1, "on_run_start should fire once");
        assert!(
            steps.lock().unwrap().contains(&"p".to_string()),
            "on_step_finish should record the output_parser node"
        );
    }

    #[tokio::test]
    async fn resume_completes_a_paused_run() {
        // trigger -> gate{requires_approval} -> downstream. Running with no
        // approvals pauses at the gate; `resume` supplies the gate approval and
        // drives the run to completion so the downstream node executes.
        let gate = |id: &str| {
            let mut gate = node(id, NodeKind::OutputParser);
            gate.config = json!({ "requires_approval": true });
            gate
        };
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate("gate"),
                node("downstream", NodeKind::OutputParser),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "gate".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "main".to_string(),
                    to_node: "downstream".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let paused = run(&compiled, json!({}), &caps).await.expect("run");
        assert!(
            paused.pending_approvals.contains(&"gate".to_string()),
            "gate should be reported as pending approval"
        );
        assert!(
            paused.output["nodes"]["downstream"].is_null(),
            "downstream must not run while the gate is pending"
        );

        let done = resume(&compiled, json!({}), vec!["gate".to_string()], &caps)
            .await
            .expect("resume");
        assert!(
            done.pending_approvals.is_empty(),
            "no approvals should be pending once the gate is approved"
        );
        assert!(
            !done.output["nodes"]["downstream"]["items"].is_null(),
            "downstream should run once the gate is approved via resume"
        );
    }

    #[tokio::test]
    async fn resume_unions_new_approval_with_existing() {
        // Two gates in series, each requiring approval. Start with `other` already
        // approved in the input and resume with `gate`: the union must preserve
        // `other` (so its gate runs) and add `gate` (so its gate runs too),
        // letting the run reach the downstream node.
        let gate = |id: &str| {
            let mut gate = node(id, NodeKind::OutputParser);
            gate.config = json!({ "requires_approval": true });
            gate
        };
        let edge = |from: &str, to: &str| Edge {
            from_node: from.to_string(),
            from_port: "main".to_string(),
            to_node: to.to_string(),
            to_port: "main".to_string(),
        };
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate("other"),
                gate("gate"),
                node("downstream", NodeKind::OutputParser),
            ],
            edges: vec![
                edge("t", "other"),
                edge("other", "gate"),
                edge("gate", "downstream"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let done = resume(
            &compiled,
            json!({ "approvals": ["other"] }),
            vec!["gate".to_string()],
            &caps,
        )
        .await
        .expect("resume");
        assert!(
            done.pending_approvals.is_empty(),
            "unioning `gate` into the existing `other` approval should clear both gates, \
             got pending: {:?}",
            done.pending_approvals
        );
        assert!(
            !done.output["nodes"]["downstream"]["items"].is_null(),
            "downstream should run once both gates are approved via the unioned set"
        );
    }

    #[tokio::test]
    async fn resumable_run_resumes_from_checkpoint() {
        // trigger -> gate{requires_approval} -> downstream. `run_resumable` pauses
        // at the gate and keeps the compiled graph (and its checkpointer) alive;
        // `ResumableRun::resume` then continues *from the checkpoint* — the gate is
        // approved via the delivered resume value and the downstream runs, without
        // re-executing the already-completed trigger.
        let mut gate = node("gate", NodeKind::OutputParser);
        gate.config = json!({ "requires_approval": true });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate,
                node("downstream", NodeKind::OutputParser),
            ],
            edges: vec![
                Edge {
                    from_node: "t".to_string(),
                    from_port: "main".to_string(),
                    to_node: "gate".to_string(),
                    to_port: "main".to_string(),
                },
                Edge {
                    from_node: "gate".to_string(),
                    from_port: "main".to_string(),
                    to_node: "downstream".to_string(),
                    to_port: "main".to_string(),
                },
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let rr = run_resumable(&compiled, json!({}), &caps)
            .await
            .expect("run_resumable");
        assert!(
            rr.outcome().pending_approvals.contains(&"gate".to_string()),
            "gate should be reported as pending approval"
        );
        assert!(
            rr.outcome().output["nodes"]["downstream"].is_null(),
            "downstream must not run while the gate is pending"
        );

        let done = rr.resume(vec!["gate".to_string()]).await.expect("resume");
        assert!(
            done.pending_approvals.is_empty(),
            "no approvals should be pending once the gate is resumed, got: {:?}",
            done.pending_approvals
        );
        assert!(
            !done.output["nodes"]["downstream"]["items"].is_null(),
            "downstream should run once the run resumes from the checkpoint"
        );
    }

    // ---- Additional comprehensive coverage ----------------------------------

    /// A `main`-port edge from `from` to `to` — the common wiring in these tests.
    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from_node: from.to_string(),
            from_port: "main".to_string(),
            to_node: to.to_string(),
            to_port: "main".to_string(),
        }
    }

    /// A `port`-port edge, for branching nodes that emit on a named port.
    fn port_edge(from: &str, port: &str, to: &str) -> Edge {
        Edge {
            from_node: from.to_string(),
            from_port: port.to_string(),
            to_node: to.to_string(),
            to_port: "main".to_string(),
        }
    }

    /// An `output_parser` gate that requires human approval before it runs.
    fn gate(id: &str) -> Node {
        let mut g = node(id, NodeKind::OutputParser);
        g.config = json!({ "requires_approval": true });
        g
    }

    #[tokio::test]
    async fn linear_three_node_passthrough() {
        // trigger -> a -> b -> c, all output_parser passthroughs. The trigger
        // payload must flow unchanged all the way to the terminal node.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("a", NodeKind::OutputParser),
                node("b", NodeKind::OutputParser),
                node("c", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "a"), edge("a", "b"), edge("b", "c")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "n": 1 }), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["a"]["items"][0]["json"],
            json!({ "n": 1 })
        );
        assert_eq!(
            outcome.output["nodes"]["b"]["items"][0]["json"],
            json!({ "n": 1 })
        );
        assert_eq!(
            outcome.output["nodes"]["c"]["items"][0]["json"],
            json!({ "n": 1 })
        );
    }

    #[tokio::test]
    async fn condition_truthy_takes_true_branch_only() {
        // condition(field=active) with a truthy input runs only the `true` branch.
        let mut condition = node("c", NodeKind::Condition);
        condition.config = json!({ "field": "active" });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                condition,
                node("yes", NodeKind::OutputParser),
                node("no", NodeKind::OutputParser),
            ],
            edges: vec![
                edge("t", "c"),
                port_edge("c", "true", "yes"),
                port_edge("c", "false", "no"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "active": true }), &caps)
            .await
            .expect("run");
        assert!(
            !outcome.output["nodes"]["yes"]["items"].is_null(),
            "true branch must run for a truthy input"
        );
        assert!(
            outcome.output["nodes"]["no"].is_null(),
            "false branch must not run for a truthy input"
        );
    }

    #[tokio::test]
    async fn condition_falsey_takes_false_branch_only() {
        // condition(field=active) with a falsey input runs only the `false` branch.
        let mut condition = node("c", NodeKind::Condition);
        condition.config = json!({ "field": "active" });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                condition,
                node("yes", NodeKind::OutputParser),
                node("no", NodeKind::OutputParser),
            ],
            edges: vec![
                edge("t", "c"),
                port_edge("c", "true", "yes"),
                port_edge("c", "false", "no"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "active": false }), &caps)
            .await
            .expect("run");
        assert!(
            outcome.output["nodes"]["yes"].is_null(),
            "true branch must not run for a falsey input"
        );
        assert!(
            !outcome.output["nodes"]["no"]["items"].is_null(),
            "false branch must run for a falsey input"
        );
    }

    #[tokio::test]
    async fn condition_without_field_uses_whole_item() {
        // No `field`: the whole (non-empty) input item is the truthiness subject,
        // so a non-empty object routes to the `true` branch.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("c", NodeKind::Condition),
                node("yes", NodeKind::OutputParser),
                node("no", NodeKind::OutputParser),
            ],
            edges: vec![
                edge("t", "c"),
                port_edge("c", "true", "yes"),
                port_edge("c", "false", "no"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
        assert!(
            !outcome.output["nodes"]["yes"]["items"].is_null(),
            "a non-empty object item is truthy and routes true"
        );
        assert!(
            outcome.output["nodes"]["no"].is_null(),
            "false branch must not run"
        );
    }

    #[tokio::test]
    async fn switch_field_matching_case_routes_there() {
        // switch(field=kind) with input kind="a" routes only to the `a` case; the
        // `default` fallback does not run.
        let mut switch = node("sw", NodeKind::Switch);
        switch.config = json!({ "field": "kind" });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                switch,
                node("case_a", NodeKind::OutputParser),
                node("fallback", NodeKind::OutputParser),
            ],
            edges: vec![
                edge("t", "sw"),
                port_edge("sw", "a", "case_a"),
                port_edge("sw", "default", "fallback"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "kind": "a" }), &caps)
            .await
            .expect("run");
        assert!(
            !outcome.output["nodes"]["case_a"]["items"].is_null(),
            "matching `a` case must run"
        );
        assert!(
            outcome.output["nodes"]["fallback"].is_null(),
            "default fallback must not run when a case matches"
        );
    }

    #[tokio::test]
    async fn switch_no_match_routes_to_default() {
        // switch(field=kind) with a missing `kind` yields a null case value, which
        // the impl maps to the `default` port; only the fallback runs.
        let mut switch = node("sw", NodeKind::Switch);
        switch.config = json!({ "field": "kind" });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                switch,
                node("case_a", NodeKind::OutputParser),
                node("fallback", NodeKind::OutputParser),
            ],
            edges: vec![
                edge("t", "sw"),
                port_edge("sw", "a", "case_a"),
                port_edge("sw", "default", "fallback"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "other": "z" }), &caps)
            .await
            .expect("run");
        assert!(
            outcome.output["nodes"]["case_a"].is_null(),
            "no case matches, so the `a` branch must not run"
        );
        assert!(
            !outcome.output["nodes"]["fallback"]["items"].is_null(),
            "a null case value routes to the default fallback"
        );
    }

    #[tokio::test]
    async fn parallel_fan_out_of_three_runs_all() {
        // trigger fans out on port `main` to three successors; all must run.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("a", NodeKind::OutputParser),
                node("b", NodeKind::OutputParser),
                node("c", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "a"), edge("t", "b"), edge("t", "c")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");
        for id in ["a", "b", "c"] {
            assert!(
                !outcome.output["nodes"][id]["items"].is_null(),
                "fan-out branch {id} should have run"
            );
        }
    }

    #[tokio::test]
    async fn merge_fan_in_concatenates_three_items() {
        // trigger -> d, which fans out to a, b, c (each a passthrough of the single
        // trigger item); all three feed merge `m`. The barrier holds until all
        // three complete, and merge concatenates their items => exactly 3 items.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("d", NodeKind::OutputParser),
                node("a", NodeKind::OutputParser),
                node("b", NodeKind::OutputParser),
                node("c", NodeKind::OutputParser),
                node("m", NodeKind::Merge),
            ],
            edges: vec![
                edge("t", "d"),
                edge("d", "a"),
                edge("d", "b"),
                edge("d", "c"),
                edge("a", "m"),
                edge("b", "m"),
                edge("c", "m"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");
        let merged = outcome.output["nodes"]["m"]["items"]
            .as_array()
            .expect("merge should have produced an items array");
        assert_eq!(
            merged.len(),
            3,
            "merge should concatenate one item from each of the 3 branches"
        );
    }

    #[tokio::test]
    async fn diamond_merge_produces_two_items() {
        // Diamond: trigger -> d, d fans out to a & b, both merge at m, then m -> done.
        // The merge sees exactly 2 items and passes them to the node past the barrier.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("d", NodeKind::OutputParser),
                node("a", NodeKind::OutputParser),
                node("b", NodeKind::OutputParser),
                node("m", NodeKind::Merge),
                node("done", NodeKind::OutputParser),
            ],
            edges: vec![
                edge("t", "d"),
                edge("d", "a"),
                edge("d", "b"),
                edge("a", "m"),
                edge("b", "m"),
                edge("m", "done"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "v": 1 }), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["m"]["items"]
                .as_array()
                .expect("merge items")
                .len(),
            2,
            "two branches merge into two items"
        );
        assert_eq!(
            outcome.output["nodes"]["done"]["items"]
                .as_array()
                .expect("done items")
                .len(),
            2,
            "the node past the barrier receives both merged items"
        );
    }

    #[tokio::test]
    async fn on_error_stop_fails_the_run() {
        // A tool_call with no `slug` errors deterministically; the default `stop`
        // policy makes the whole run return Err.
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("x", NodeKind::ToolCall)],
            edges: vec![edge("t", "x")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        assert!(
            run(&compiled, json!({}), &caps).await.is_err(),
            "a failing node under the default stop policy must fail the run"
        );
    }

    #[tokio::test]
    async fn on_error_continue_completes_with_error_item() {
        // `on_error: continue` turns the failure into an error item on the default
        // port and lets the run complete Ok.
        let mut tool = node("x", NodeKind::ToolCall);
        tool.config = json!({ "on_error": "continue" });
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), tool],
            edges: vec![edge("t", "x")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({}), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["x"]["items"][0]["json"]["error"]["node"],
            json!("x")
        );
    }

    #[tokio::test]
    async fn on_error_route_delivers_error_item_to_recovery_node() {
        // `on_error: route` emits the error item on the `error` port; a recovery
        // node wired from that port receives it, and the main-port branch does not.
        let mut tool = node("x", NodeKind::ToolCall);
        tool.config = json!({ "on_error": "route" });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                tool,
                node("recover", NodeKind::OutputParser),
                node("normal", NodeKind::OutputParser),
            ],
            edges: vec![
                edge("t", "x"),
                port_edge("x", "error", "recover"),
                port_edge("x", "main", "normal"),
            ],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({}), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["recover"]["items"][0]["json"]["error"]["node"],
            json!("x"),
            "recovery node must receive the routed error item"
        );
        assert!(
            outcome.output["nodes"]["normal"].is_null(),
            "the main branch must not run when the error routes to `error`"
        );
    }

    #[tokio::test]
    async fn error_item_has_node_and_message_fields() {
        // Assert the concrete shape of the emitted error item: json.error carries a
        // `node` (the failing node id) and a non-empty `message`.
        let mut tool = node("x", NodeKind::ToolCall);
        tool.config = json!({ "on_error": "continue" });
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), tool],
            edges: vec![edge("t", "x")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({}), &caps).await.expect("run");
        let err = &outcome.output["nodes"]["x"]["items"][0]["json"]["error"];
        assert_eq!(err["node"], json!("x"));
        assert!(
            err["message"].as_str().is_some_and(|m| !m.is_empty()),
            "error item must carry a non-empty message, got {err:?}"
        );
    }

    #[tokio::test]
    async fn retry_max_attempts_then_continue_completes() {
        // `retry.max_attempts` retries the failing node; after they are exhausted,
        // `on_error: continue` yields the error item and the run completes.
        let mut tool = node("x", NodeKind::ToolCall);
        tool.config = json!({ "retry": { "max_attempts": 4 }, "on_error": "continue" });
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), tool],
            edges: vec![edge("t", "x")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({}), &caps).await.expect("run");
        let err = &outcome.output["nodes"]["x"]["items"][0]["json"]["error"];
        assert_eq!(err["node"], json!("x"));
        assert!(err["message"].as_str().is_some_and(|m| !m.is_empty()));
    }

    #[tokio::test]
    async fn hitl_gate_pauses_and_blocks_downstream() {
        // A requires_approval gate with no approval pauses the run: reported pending
        // and its downstream never runs.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate("g"),
                node("downstream", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "g"), edge("g", "downstream")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
        assert!(
            outcome.pending_approvals.contains(&"g".to_string()),
            "gate should be reported pending"
        );
        assert!(
            outcome.output["nodes"]["downstream"].is_null(),
            "downstream must not run behind a pending gate"
        );
    }

    #[tokio::test]
    async fn hitl_two_gates_resume_one_leaves_next_pending() {
        // Two sequential gates: g1 -> g2 -> done. Resuming g1 lets g2 become the new
        // pending gate (done still blocked); a second resume of g2 completes the run.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate("g1"),
                gate("g2"),
                node("done", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "g1"), edge("g1", "g2"), edge("g2", "done")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let rr = run_resumable(&compiled, json!({}), &caps)
            .await
            .expect("run_resumable");
        assert!(
            rr.outcome().pending_approvals.contains(&"g1".to_string()),
            "g1 should be the first pending gate"
        );
        assert!(
            !rr.outcome().pending_approvals.contains(&"g2".to_string()),
            "g2 is not reached until g1 is approved"
        );

        let after_g1 = rr.resume(vec!["g1".to_string()]).await.expect("resume g1");
        assert!(
            after_g1.pending_approvals.contains(&"g2".to_string()),
            "g2 becomes pending after g1 is approved, got {:?}",
            after_g1.pending_approvals
        );
        assert!(
            after_g1.output["nodes"]["done"].is_null(),
            "done stays blocked while g2 is pending"
        );

        let done = rr.resume(vec!["g2".to_string()]).await.expect("resume g2");
        assert!(
            done.pending_approvals.is_empty(),
            "no gate pending once both are approved, got {:?}",
            done.pending_approvals
        );
        assert!(
            !done.output["nodes"]["done"]["items"].is_null(),
            "done runs once both gates are approved"
        );
    }

    #[tokio::test]
    async fn approval_via_input_proceeds_immediately() {
        // Listing the gate id in the run input's `approvals` lets it proceed on the
        // first run with no pause: nothing pending, gate and downstream both run.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate("g"),
                node("downstream", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "g"), edge("g", "downstream")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "approvals": ["g"] }), &caps)
            .await
            .expect("run");
        assert!(
            outcome.pending_approvals.is_empty(),
            "an input-approved gate leaves nothing pending"
        );
        assert!(
            !outcome.output["nodes"]["g"]["items"].is_null(),
            "the approved gate itself runs"
        );
        assert!(
            !outcome.output["nodes"]["downstream"]["items"].is_null(),
            "downstream runs once the gate is approved via input"
        );
    }

    /// A [`RunObserver`] that counts run-start / run-finish and records step ids,
    /// so a test can assert every hook fired the right number of times.
    #[derive(Default)]
    struct FullCapture {
        starts: Mutex<u32>,
        finishes: Mutex<u32>,
        steps: Mutex<Vec<String>>,
    }

    impl RunObserver for FullCapture {
        fn on_run_start(&self, _run_id: &str) {
            *self.starts.lock().unwrap() += 1;
        }

        fn on_step_finish(&self, step: &ExecutionStep) {
            self.steps.lock().unwrap().push(step.node_id.clone());
        }

        fn on_run_finish(&self, _run: &Run) {
            *self.finishes.lock().unwrap() += 1;
        }
    }

    #[tokio::test]
    async fn observer_fires_start_finish_and_run_finish_counts() {
        // trigger -> a -> b. on_run_start fires once, on_run_finish once, and
        // on_step_finish fires once per non-trigger node (a, b) — never the trigger.
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("a", NodeKind::OutputParser),
                node("b", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "a"), edge("a", "b")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let capture = Arc::new(FullCapture::default());
        let observer: Arc<dyn RunObserver> = capture.clone();
        run_with_observer(&compiled, json!({ "x": 1 }), &caps, &observer)
            .await
            .expect("run");

        assert_eq!(*capture.starts.lock().unwrap(), 1, "on_run_start once");
        assert_eq!(*capture.finishes.lock().unwrap(), 1, "on_run_finish once");
        let steps = capture.steps.lock().unwrap();
        assert_eq!(steps.len(), 2, "one step per non-trigger node");
        assert!(steps.contains(&"a".to_string()));
        assert!(steps.contains(&"b".to_string()));
        assert!(
            !steps.contains(&"t".to_string()),
            "the trigger must not produce a step"
        );
    }

    #[tokio::test]
    async fn run_level_knobs_do_not_break_execution() {
        // A trigger carrying run-level recursion_limit + node_timeout_secs drives a
        // multi-node chain to completion, proving the knobs are wired without harm.
        let mut trigger = node("t", NodeKind::Trigger);
        trigger.config = json!({ "recursion_limit": 100, "node_timeout_secs": 30 });
        let graph = WorkflowGraph {
            nodes: vec![
                trigger,
                node("a", NodeKind::OutputParser),
                node("b", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "a"), edge("a", "b")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["b"]["items"][0]["json"],
            json!({ "x": 1 })
        );
    }

    #[tokio::test]
    async fn trigger_only_completes_cleanly() {
        // A lone trigger runs to completion with nothing pending and its payload
        // seeded as the trigger node's single item.
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger)],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");
        let caps = mock_capabilities();

        let outcome = run(&compiled, json!({ "seed": 7 }), &caps)
            .await
            .expect("run");
        assert!(
            outcome.pending_approvals.is_empty(),
            "a trigger-only run has nothing pending"
        );
        assert_eq!(
            outcome.output["nodes"]["t"]["items"][0]["json"],
            json!({ "seed": 7 })
        );
    }

    // ---- Host-injectable checkpointer -------------------------------------

    /// Compile-time proof that the handles a host holds across the gap between
    /// run and resume are thread-safe: [`ResumableRun`] (kept alive across a
    /// HITL pause) and [`RunOutcome`] (returned from every entry point) must be
    /// `Send + Sync` so a host can move them between tasks/threads.
    #[test]
    fn resumable_run_and_outcome_are_send_sync() {
        fn _assert<T: Send + Sync>() {}
        _assert::<ResumableRun>();
        _assert::<RunOutcome>();
    }

    #[tokio::test]
    async fn durable_resume_via_injected_checkpointer() {
        // A SHARED, externally-held checkpointer simulates a host's durable store
        // that survives across "processes": we run under it, then rebuild caps +
        // graph and resume from it by thread id alone.
        let cp: Arc<dyn Checkpointer<Value>> = Arc::new(InMemoryCheckpointer::<Value>::default());

        // trigger -> gate{requires_approval} -> downstream(output_parser).
        let mut gate = node("gate", NodeKind::OutputParser);
        gate.config = json!({ "requires_approval": true });
        let graph = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate,
                node("downstream", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "gate"), edge("gate", "downstream")],
            ..Default::default()
        };
        let compiled = compile(&graph).expect("compile");

        // "Process 1": run under the host checkpointer, pausing at the gate.
        let caps = mock_capabilities();
        let paused = run_with_checkpointer(&compiled, json!({}), &caps, cp.clone(), "thread-A")
            .await
            .expect("run_with_checkpointer");
        assert_eq!(
            paused.pending_approvals,
            vec!["gate".to_string()],
            "the gate must be reported pending"
        );
        assert!(
            paused.output["nodes"]["downstream"].is_null(),
            "downstream must not run behind a pending gate"
        );

        // "Process 2": fresh caps, same durable checkpointer + thread id.
        let caps = mock_capabilities();
        let done = resume_with_checkpointer(
            &compiled,
            &caps,
            cp.clone(),
            "thread-A",
            vec!["gate".to_string()],
        )
        .await
        .expect("resume_with_checkpointer");
        assert!(
            done.pending_approvals.is_empty(),
            "nothing should be pending once the gate is approved, got {:?}",
            done.pending_approvals
        );
        assert!(
            !done.output["nodes"]["downstream"]["items"].is_null(),
            "downstream should run once the run resumes from the durable checkpoint"
        );
    }

    #[tokio::test]
    async fn plain_run_and_resumable_unchanged_by_injectable_checkpointer() {
        // Regression: the default (non-injectable) `run` and `run_resumable`
        // paths must behave exactly as before. `run` drives a linear passthrough
        // to completion; `run_resumable` pauses at a gate and resumes from its
        // own in-memory checkpoint.
        let linear = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                node("p", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "p")],
            ..Default::default()
        };
        let compiled = compile(&linear).expect("compile");
        let caps = mock_capabilities();
        let outcome = run(&compiled, json!({ "x": 1 }), &caps).await.expect("run");
        assert_eq!(
            outcome.output["nodes"]["p"]["items"][0]["json"],
            json!({ "x": 1 })
        );
        assert!(outcome.pending_approvals.is_empty());

        let mut gate = node("gate", NodeKind::OutputParser);
        gate.config = json!({ "requires_approval": true });
        let gated = WorkflowGraph {
            nodes: vec![
                node("t", NodeKind::Trigger),
                gate,
                node("downstream", NodeKind::OutputParser),
            ],
            edges: vec![edge("t", "gate"), edge("gate", "downstream")],
            ..Default::default()
        };
        let compiled = compile(&gated).expect("compile");
        let caps = mock_capabilities();
        let rr = run_resumable(&compiled, json!({}), &caps)
            .await
            .expect("run_resumable");
        assert!(rr.outcome().pending_approvals.contains(&"gate".to_string()));
        assert!(rr.outcome().output["nodes"]["downstream"].is_null());
        let done = rr.resume(vec!["gate".to_string()]).await.expect("resume");
        assert!(done.pending_approvals.is_empty());
        assert!(!done.output["nodes"]["downstream"]["items"].is_null());
    }
}
