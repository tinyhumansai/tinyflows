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
    Command, END, GraphBuilder, InMemoryCheckpointer, Interrupt, NodeResult, StateReducer,
    TinyAgentsError,
};

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
    let graph = &workflow.graph;

    // Process-local, monotonic run id — no time/random source.
    let run_id = format!("run-{}", NEXT_RUN_ID.fetch_add(1, Ordering::Relaxed));
    observer.on_run_start(&run_id);

    // Node handlers stream finished steps here (they also fire
    // `on_step_finish`); the engine folds this into the final `Run`. A shared
    // `Mutex` is the simplest correct sink across the `'static + Send + Sync`
    // handler closures, which can't otherwise push to a common `Vec`.
    let steps: Arc<Mutex<Vec<ExecutionStep>>> = Arc::new(Mutex::new(Vec::new()));

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

        builder = builder.add_node(node.id.clone(), move |state: Value, _ctx| {
            let node = node.clone();
            let predecessors = predecessors.clone();
            let caps = caps.clone();
            let observer = observer.clone();
            let steps = steps.clone();
            let fan_out = fan_out.clone();
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
                    if !approved {
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

    // A checkpointer (plus a thread id on the run below) is required for
    // tinyagents to persist the interrupt boundary and hand pending approvals
    // back to us; an in-memory one keeps the crate host-agnostic and dep-free.
    let compiled = builder
        .compile()
        .map_err(|e| EngineError::Capability(e.to_string()))?
        .with_checkpointer(Arc::new(InMemoryCheckpointer::<Value>::default()));

    let seed_items = items_update(&trigger_id, &[Item::new(input.clone())], None)
        .map_err(|e| EngineError::Capability(e.to_string()))?;
    let mut initial = json!({ "run": { "trigger": input } });
    merge(&mut initial, seed_items);

    let execution = compiled
        .run_with_thread(trigger_id.clone(), initial)
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

    Ok(RunOutcome {
        output: execution.state,
        pending_approvals,
    })
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
}
