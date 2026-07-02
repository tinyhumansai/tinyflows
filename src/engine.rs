//! Drives a [`CompiledWorkflow`] to completion by lowering it onto `tinyagents`.
//!
//! `run` builds a fresh `tinyagents` state graph from the validated
//! [`WorkflowGraph`](crate::model::WorkflowGraph) — capturing the run's host
//! [`Capabilities`] in each node handler — then drives it and returns the final
//! run state. State is a [`serde_json::Value`] laid out as
//! `{ "run": { "trigger": … }, "nodes": { "<id>": { "items": [ … ] } } }`;
//! a merge reducer folds each node's item output into that map.
//!
//! Stage A1 lowers the **linear** path (one successor per node). Branching,
//! parallel, and fan-in lowering land in A2. See `docs/04-execution-engine.md`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use serde_json::{Map, Value, json};
use tinyagents::{
    END, GraphBuilder, InMemoryCheckpointer, Interrupt, NodeResult, StateReducer, TinyAgentsError,
};

use crate::caps::Capabilities;
use crate::compiler::CompiledWorkflow;
use crate::data::Item;
use crate::error::{EngineError, Result};
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

    let trigger_id = graph
        .trigger()
        .ok_or(EngineError::Unimplemented(
            "workflow must have exactly one trigger",
        ))?
        .id
        .clone();

    tracing::info!(node_count = graph.nodes.len(), trigger = %trigger_id, "workflow run starting");

    // Branching (multiple successors on DISTINCT ports) lowers to conditional
    // edges. Two or more edges sharing a `from_port` is parallel fan-out, which
    // still needs A2 parallel lowering.
    for node in &graph.nodes {
        let mut seen_ports = std::collections::HashSet::new();
        for edge in graph.edges.iter().filter(|e| e.from_node == node.id) {
            if !seen_ports.insert(edge.from_port.as_str()) {
                return Err(EngineError::Unimplemented(
                    "parallel fan-out lowering (stage A2)",
                ));
            }
        }
    }

    let mut builder = GraphBuilder::<Value, Value>::new().set_reducer(MergeReducer);

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

        builder = builder.add_node(node.id.clone(), move |state: Value, _ctx| {
            let node = node.clone();
            let predecessors = predecessors.clone();
            let caps = caps.clone();
            let observer = observer.clone();
            let steps = steps.clone();
            async move {
                if is_trigger {
                    // The trigger payload is pre-seeded into the state; no-op update.
                    return Ok(NodeResult::Update(json!({})));
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

                // Attempt the executor up to `max_attempts` times: use the first
                // `Ok`, otherwise keep the last `Err`. Backoff timing (sleep between
                // attempts) is intentionally deferred — the library is
                // runtime-agnostic and must not depend on a timer/runtime here.
                let mut output = None;
                let mut last_err: Option<EngineError> = None;
                let started = Instant::now();
                for _ in 0..max_attempts {
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
                        Ok(NodeResult::Update(items_update(
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
                            return Ok(NodeResult::Update(items_update(&node.id, &[], None)?));
                        };
                        match on_error {
                            // Turn the failure into data on the default port.
                            "continue" => Ok(NodeResult::Update(items_update(
                                &node.id,
                                &[error_item(&node.id, &err)],
                                None,
                            )?)),
                            // Turn the failure into data on the `error` port so the
                            // graph can route it to a recovery sub-graph.
                            "route" => Ok(NodeResult::Update(items_update(
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
        match outgoing.as_slice() {
            // Leaf node: nothing routes out, so it terminates the run.
            [] => builder = builder.add_edge(node.id.clone(), END),
            // Single successor lowers to a plain static edge.
            [edge] => builder = builder.add_edge(node.id.clone(), edge.to_node.clone()),
            // Branching: distinct ports lower to conditional edges keyed on the
            // port the node recorded into state (defaulting to `main`).
            edges => {
                let from = node.id.clone();
                let routes: Vec<(String, String)> = edges
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
    async fn fan_out_is_rejected_until_a2() {
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

        let error = run(&compiled, Value::Null, &caps)
            .await
            .expect_err("fan-out");
        assert!(matches!(error, EngineError::Unimplemented(_)));
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
}
