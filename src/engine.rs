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

use serde_json::{Map, Value, json};
use tinyagents::{END, GraphBuilder, NodeResult, StateReducer, TinyAgentsError};

use crate::caps::Capabilities;
use crate::compiler::CompiledWorkflow;
use crate::data::Item;
use crate::error::{EngineError, Result};
use crate::model::NodeKind;
use crate::nodes::{NodeContext, executor_for};

/// The result of a completed workflow run.
#[derive(Debug, Clone)]
pub struct RunOutcome {
    /// The final run state after the terminal node(s) completed.
    pub output: Value,
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

/// Builds the partial state update a node contributes: `{ "nodes": { id: { items } } }`.
fn items_update(node_id: &str, items: &[Item]) -> tinyagents::Result<Value> {
    let mut slot = Map::new();
    slot.insert("items".to_string(), serde_json::to_value(items)?);
    let mut nodes = Map::new();
    nodes.insert(node_id.to_string(), Value::Object(slot));
    let mut root = Map::new();
    root.insert("nodes".to_string(), Value::Object(nodes));
    Ok(Value::Object(root))
}

/// Executes a compiled workflow with the given trigger `input` and host
/// `capabilities`, driving it to completion.
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
    let graph = &workflow.graph;

    let trigger_id = graph
        .trigger()
        .ok_or(EngineError::Unimplemented(
            "workflow must have exactly one trigger",
        ))?
        .id
        .clone();

    // A1 lowers the linear path only; fan-out needs conditional/parallel lowering.
    for node in &graph.nodes {
        if graph
            .edges
            .iter()
            .filter(|e| e.from_node == node.id)
            .count()
            > 1
        {
            return Err(EngineError::Unimplemented(
                "branching / parallel lowering (stage A2)",
            ));
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
        let is_trigger = node.kind == NodeKind::Trigger;

        builder = builder.add_node(node.id.clone(), move |state: Value, _ctx| {
            let node = node.clone();
            let predecessors = predecessors.clone();
            let caps = caps.clone();
            async move {
                if is_trigger {
                    // The trigger payload is pre-seeded into the state; no-op update.
                    return Ok(NodeResult::Update(json!({})));
                }
                let input = collect_input(&state, &predecessors);
                let run_meta = state.get("run").cloned().unwrap_or(Value::Null);
                let output = {
                    let ctx = NodeContext {
                        node: &node,
                        input: &input,
                        run: &run_meta,
                        caps: &caps,
                    };
                    executor_for(&node.kind)
                        .execute(ctx)
                        .await
                        .map_err(|e| TinyAgentsError::Graph(e.to_string()))?
                };
                Ok(NodeResult::Update(items_update(&node.id, &output.items)?))
            }
        });
    }

    for edge in &graph.edges {
        builder = builder.add_edge(edge.from_node.clone(), edge.to_node.clone());
    }
    builder = builder.set_entry(trigger_id.clone());
    for node in &graph.nodes {
        if !graph.edges.iter().any(|e| e.from_node == node.id) {
            builder = builder.add_edge(node.id.clone(), END);
        }
    }

    let compiled = builder
        .compile()
        .map_err(|e| EngineError::Capability(e.to_string()))?;

    let seed_items = items_update(&trigger_id, &[Item::new(input.clone())])
        .map_err(|e| EngineError::Capability(e.to_string()))?;
    let mut initial = json!({ "run": { "trigger": input } });
    merge(&mut initial, seed_items);

    let execution = compiled
        .run(initial)
        .await
        .map_err(|e| EngineError::Capability(e.to_string()))?;

    Ok(RunOutcome {
        output: execution.state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::caps::mock::mock_capabilities;
    use crate::compiler::compile;
    use crate::model::{Edge, Node, WorkflowGraph};

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: id.to_string(),
            kind,
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
        // trigger -> switch: the switch executor is a stage-A2 stub, so the
        // run reaches it and surfaces its error — proving edge lowering + dispatch.
        let graph = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), node("x", NodeKind::Switch)],
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

        let error = run(&compiled, Value::Null, &caps)
            .await
            .expect_err("switch stub should surface");
        assert!(
            error.to_string().contains("switch"),
            "unexpected error: {error}"
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
}
