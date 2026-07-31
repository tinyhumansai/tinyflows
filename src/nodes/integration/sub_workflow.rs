//! The `sub_workflow` node: runs another workflow as a nested sub-graph.

use async_trait::async_trait;
use serde_json::Value;

use crate::engine::MAX_SUB_WORKFLOW_DEPTH;
use crate::error::{EngineError, Result};
use crate::model::{NodeKind, WorkflowGraph};
use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

/// Runs another workflow as a nested sub-graph.
///
/// The child [`WorkflowGraph`](crate::model::WorkflowGraph) is supplied one of
/// two ways — **exactly one** of these config keys must be present:
///
/// - `workflow` — the child graph embedded **inline** as JSON (back-compat;
///   the original behavior).
/// - `workflow_id` — a host-managed **reference** to a saved workflow. The
///   engine is persistence-free, so it resolves the id to a graph through the
///   host-injected [`WorkflowResolver`](crate::caps::WorkflowResolver)
///   (`ctx.caps.resolver`).
///
/// The resolved child is compiled and run via [`crate::engine::run_sub_workflow`],
/// sharing the host [`Capabilities`](crate::caps::Capabilities) with the parent
/// run, and its final run state is emitted as an output item.
///
/// ## Execution: the multiplier
///
/// `config.execution` (default `once`) decides how many child runs this node
/// performs:
///
/// - `once` — one child run seeded with the node's whole input array.
/// - `per_item` — **one full child run per input item**, each seeded with just
///   that item and resolving `workflow_id` against it (so `=item.x` addresses
///   the element that run is for). `config.concurrency` bounds how many run at
///   a time and `config.on_item_error` what a failing child does to the batch
///   (see [`crate::nodes::map`]). This is how an array of work becomes N
///   parallel multi-step workflows.
///
/// Only the fields *this* node reads are `=`-resolved; an inline `workflow`
/// graph always passes through untouched because its expressions belong to the
/// child run.
///
/// The depth guard below is per child run, so a fan-out widens a run without
/// deepening it — N siblings at depth d+1, never d+N.
///
/// ## Passing the child's declared inputs
///
/// An optional `inputs` config object supplies values for the child's declared
/// [`WorkflowInput`](crate::model::WorkflowInput)s. Each field is resolved
/// against the parent's expression scope, so a parent can forward its own
/// inputs or an upstream node's output:
///
/// ```json
/// {
///   "workflow_id": "review-and-fix",
///   "inputs": { "repo": "=inputs.repo", "depth": 2 }
/// }
/// ```
///
/// The child validates what arrives against its own declarations, so a parent
/// that omits a required child input fails the same way a top-level caller
/// would — before the child executes anything.
///
/// ## Cycle / depth handling
///
/// Every nested `sub_workflow` run (inline or by id) increments a
/// `run.sub_workflow_depth` counter; a child that would exceed
/// [`MAX_SUB_WORKFLOW_DEPTH`] is refused. This bounds **any** cycle — including
/// indirect ones like flow A → flow B → flow A by id — after at most that many
/// levels. In addition, a **direct self-reference** (a resolved child graph that
/// itself references the same `workflow_id`) is caught statically here before the
/// child ever runs, so the common one-level loop fails fast with a clear error
/// rather than unwinding the full depth budget.
#[derive(Debug, Default, Clone)]
pub struct SubWorkflowNode;

/// Reads the current nesting depth from the run metadata (`0` at the top level).
fn current_depth(run: &Value) -> u64 {
    run.get("sub_workflow_depth")
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

/// Deserializes the inline `workflow` config value into a [`WorkflowGraph`].
fn inline_child(workflow: &Value) -> Result<WorkflowGraph> {
    serde_json::from_value(workflow.clone())
        .map_err(|e| EngineError::Capability(format!("sub_workflow node: invalid workflow: {e}")))
}

/// Rejects a resolved child that references the same `workflow_id` it was loaded
/// under — a direct self-reference (one-level cycle). Deeper cycles are still
/// bounded by the depth counter; this catches the obvious case eagerly.
fn reject_self_reference(child: &WorkflowGraph, workflow_id: &str) -> Result<()> {
    let self_ref = child
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::SubWorkflow)
        .any(|n| n.config.get("workflow_id").and_then(Value::as_str) == Some(workflow_id));
    if self_ref {
        return Err(EngineError::Capability(format!(
            "sub_workflow node: workflow_id {workflow_id:?} references itself (cycle)"
        )));
    }
    Ok(())
}

/// Builds the values passed to the child's declared inputs from this node's
/// `inputs` config object.
///
/// Each field's value is resolved against the **parent's** scope, so a parent
/// can forward its own inputs (`"repo": "=inputs.repo"`), an upstream node's
/// output (`"=nodes.fetch.item.url"`), or a literal. The child then validates
/// what arrives against its own declarations, exactly as a top-level caller
/// would — a parent that forgets a required child input fails loudly.
///
/// An absent or non-object `inputs` config yields an empty map, so a
/// `sub_workflow` node authored before inputs existed keeps working against a
/// child that declares none.
fn child_inputs(config: &Value, scope: &Value) -> Result<serde_json::Map<String, Value>> {
    let Some(declared) = config.get("inputs") else {
        return Ok(serde_json::Map::new());
    };
    let Some(fields) = declared.as_object() else {
        return Err(EngineError::Capability(
            "sub_workflow node: `inputs` must be an object mapping the child's declared input \
             names to values"
                .to_string(),
        ));
    };
    Ok(fields
        .iter()
        .map(|(name, value)| (name.clone(), crate::expr::resolve(value, scope)))
        .collect())
}

#[async_trait]
impl NodeExecutor for SubWorkflowNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        // Execution mode (default `once`): `once` runs the child graph a single
        // time with the node's whole input array as its payload. `per_item`
        // makes this node the **multiplier** — one full child run per input
        // item, each seeded with just that item, bounded by
        // `config.concurrency`. That is what turns an array of work into N
        // parallel multi-step workflows.
        let per_item =
            crate::nodes::execution_mode(&ctx.node.config, crate::nodes::ExecutionMode::Once)
                == crate::nodes::ExecutionMode::PerItem
                && !ctx.input.is_empty();

        if per_item {
            let opts = crate::nodes::map::map_options(&ctx.node.config, &ctx.node.id);
            let ctx = &ctx;
            let (items, _) =
                crate::nodes::map::map_items(ctx.input.len(), opts, move |index| async move {
                    let item = &ctx.input[index];
                    // Each child resolves `workflow_id` against *its own* item,
                    // so `=item.x` addresses the element this run is for, and
                    // receives that single item as its input.
                    let scope = crate::nodes::expr_scope_for(ctx, item.json.clone());
                    let child = run_child(ctx, &scope, std::slice::from_ref(item)).await?;
                    Ok((child, vec![]))
                })
                .await?;
            return Ok(NodeOutput::main(items));
        }

        let scope = crate::nodes::expr_scope(&ctx);
        let item = run_child(&ctx, &scope, ctx.input).await?;
        Ok(NodeOutput::main(vec![item]))
    }
}

/// Resolves this node's child graph and runs it once, returning the child's
/// final run state as a single [`Item`](crate::data::Item).
///
/// `scope` is the expression scope `workflow_id` is resolved against (the whole
/// input for `once`, the current element for `per_item`), and `child_input` is
/// the item array seeded into the child run.
async fn run_child(
    ctx: &NodeContext<'_>,
    scope: &Value,
    child_input: &[crate::data::Item],
) -> Result<crate::data::Item> {
    // The inline `workflow` graph carries its *own* `=`-expressions, scoped
    // to the CHILD run — it must pass through untouched. Only the fields the
    // sub_workflow node itself reads (here `workflow_id`) are resolved
    // against this node's input scope, mirroring every other integration
    // node (see `tool_call`).
    let inline = ctx.node.config.get("workflow");
    let resolved_workflow_id = ctx
        .node
        .config
        .get("workflow_id")
        .map(|v| crate::expr::resolve(v, scope));
    let workflow_id = resolved_workflow_id
        .as_ref()
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());

    // Exactly one of `workflow` / `workflow_id` must be set.
    let child: WorkflowGraph = match (inline, workflow_id) {
        (Some(_), Some(_)) => {
            return Err(EngineError::Capability(
                "sub_workflow node: set exactly one of `workflow` (inline) or `workflow_id` \
                 (reference), not both"
                    .to_string(),
            ));
        }
        (None, None) => {
            return Err(EngineError::Capability(
                "sub_workflow node: missing `workflow` (inline) or `workflow_id` (reference) \
                 in config"
                    .to_string(),
            ));
        }
        (Some(inline_value), None) => {
            tracing::debug!(node = %ctx.node.id, "sub_workflow: running inline child graph");
            inline_child(inline_value)?
        }
        (None, Some(id)) => {
            tracing::debug!(node = %ctx.node.id, workflow_id = %id, "sub_workflow: resolving child graph by workflow_id");
            let resolved = ctx.caps.resolver.resolve(id).await?;
            reject_self_reference(&resolved, id)?;
            resolved
        }
    };

    // Depth / cycle guard: bound total nesting regardless of how a cycle is
    // formed. The child runs one level deeper than the current run.
    let child_depth = current_depth(ctx.run) + 1;
    if child_depth > MAX_SUB_WORKFLOW_DEPTH {
        return Err(EngineError::Capability(format!(
            "sub_workflow node: maximum nesting depth {MAX_SUB_WORKFLOW_DEPTH} exceeded \
             (possible cycle)"
        )));
    }

    let compiled = crate::compiler::compile(&child)?;
    let trigger =
        serde_json::to_value(child_input).map_err(|e| EngineError::Capability(e.to_string()))?;
    // Resolved against the same `scope` as `workflow_id`, so a `per_item` run
    // forwards values derived from *its* element (`"=item.repo"`) rather than
    // from the batch — the whole point of resolving inputs in here rather than
    // once at the call site.
    let child_inputs = child_inputs(&ctx.node.config, scope)?;
    // Box the recursive engine call so the async future type stays sized.
    let outcome = Box::pin(crate::engine::run_sub_workflow(
        &compiled,
        crate::engine::RunInput::new(trigger).with_inputs(child_inputs),
        ctx.caps,
        child_depth,
    ))
    .await?;

    // Enforce the child's lifecycle across the sub-workflow boundary (BUG-5).
    //
    // The child run is a *separate* engine invocation whose non-completion is
    // reported on its [`RunOutcome`], not on the [`NodeOutput`] this node
    // returns. A node executor has no channel to inject a tinyagents interrupt
    // into the *parent* run (the parent's `pending_approvals` are collected
    // solely from its own boundary interrupts), so we cannot yet transparently
    // pause the parent and resume the child at its gate. What we MUST NOT do is
    // keep only `outcome.output` and report success — that silently treats a
    // child that paused at a `requires_approval` gate (or was cancelled) as if
    // it had run to completion, making approval gating unenforceable across the
    // boundary.
    //
    // Until full cross-boundary resume exists, fail loudly: a child that did
    // not fully complete halts the parent with an error rather than letting it
    // falsely complete. With the default `on_error: stop` policy this stops the
    // parent run; with `continue`/`route` it becomes a routable error item —
    // either way the gated child is never silently treated as completed.
    //
    // Follow-up for full cross-boundary resume: surface the child's
    // `pending_approvals` (namespaced by this node's id) into the parent's
    // pending set via a real interrupt at this node's boundary, and teach
    // `engine::resume` to re-enter the child at its paused gate. That needs
    // engine-level interrupt plumbing this node cannot express today.
    if !outcome.pending_approvals.is_empty() {
        return Err(EngineError::Capability(format!(
            "sub_workflow node {:?}: child run paused awaiting approval at {:?}; \
             cross-boundary approval resume is not yet supported, so the parent run is \
             halted rather than falsely completed",
            ctx.node.id, outcome.pending_approvals
        )));
    }
    if outcome.cancelled {
        return Err(EngineError::Capability(format!(
            "sub_workflow node {:?}: child run was cancelled before completing; the parent \
             run is halted rather than falsely completed",
            ctx.node.id
        )));
    }

    Ok(crate::data::Item::new(outcome.output))
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::SubWorkflowNode;
    use crate::caps::Capabilities;
    use crate::caps::mock::{
        MockWorkflowResolver, mock_capabilities, mock_capabilities_with_resolver,
    };
    use crate::compiler::compile;
    use crate::engine::run;
    use crate::error::EngineError;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};
    use crate::nodes::{NodeContext, NodeExecutor, NodeOutput};

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

    async fn execute_err(config: Value) -> EngineError {
        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = config;
        let input = vec![];
        let caps = mock_capabilities();
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &sw,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps: &caps,
        };
        SubWorkflowNode
            .execute(ctx)
            .await
            .expect_err("expected an error")
    }

    /// Runs a `sub_workflow` node with the given config over `input_items`.
    async fn execute_over(
        config: Value,
        input_items: Vec<crate::data::Item>,
        caps: &Capabilities,
    ) -> NodeOutput {
        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = config;
        let run_meta = Value::Null;
        let ctx = NodeContext {
            node: &sw,
            input: &input_items,
            run: &run_meta,
            nodes: &Value::Null,
            caps,
        };
        SubWorkflowNode.execute(ctx).await.expect("execute")
    }

    /// A child graph whose trigger simply carries the payload it was seeded with.
    fn passthrough_child() -> WorkflowGraph {
        WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger)],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn per_item_runs_the_child_graph_once_per_input_item() {
        // The multiplier: three items in, three complete child runs out.
        let caps = mock_capabilities_with_resolver(
            MockWorkflowResolver::default().with("child-1", passthrough_child()),
        );
        let input = vec![
            crate::data::Item::new(json!({ "topic": "a" })),
            crate::data::Item::new(json!({ "topic": "b" })),
            crate::data::Item::new(json!({ "topic": "c" })),
        ];
        let out = execute_over(
            json!({ "workflow_id": "child-1", "execution": "per_item", "concurrency": 3 }),
            input,
            &caps,
        )
        .await;

        assert_eq!(out.items.len(), 3, "one child run per input item");
        for (index, item) in out.items.iter().enumerate() {
            assert_eq!(item.paired_item, Some(index), "output pairs to its input");
        }
        // Each child was seeded with ONLY its own item: its trigger payload is a
        // one-element item array, not the parent's whole input.
        for item in &out.items {
            assert_eq!(
                item.json["run"]["trigger"]
                    .as_array()
                    .expect("trigger items")
                    .len(),
                1,
                "each child sees exactly its own item"
            );
        }
        let topics: Vec<&str> = out
            .items
            .iter()
            .map(|i| {
                i.json["run"]["trigger"][0]["json"]["topic"]
                    .as_str()
                    .expect("topic")
            })
            .collect();
        assert_eq!(topics, ["a", "b", "c"], "children keep input order");
    }

    #[tokio::test]
    async fn once_is_still_the_default_and_seeds_the_whole_input_array() {
        // Back-compat: without `execution` the node runs a single child seeded
        // with every input item, exactly as before fan-out existed.
        let caps = mock_capabilities_with_resolver(
            MockWorkflowResolver::default().with("child-1", passthrough_child()),
        );
        let input = vec![
            crate::data::Item::new(json!({ "topic": "a" })),
            crate::data::Item::new(json!({ "topic": "b" })),
        ];
        let out = execute_over(json!({ "workflow_id": "child-1" }), input, &caps).await;

        assert_eq!(
            out.items.len(),
            1,
            "one child run regardless of input count"
        );
        let seeded = &out.items[0].json["run"]["trigger"];
        assert_eq!(
            seeded.as_array().expect("trigger items").len(),
            2,
            "the single child is seeded with the whole input array"
        );
    }

    #[tokio::test]
    async fn per_item_resolves_workflow_id_against_the_current_item() {
        // `=item.x` in `workflow_id` addresses the element this child run is
        // for, so one node can dispatch each item to a different child graph.
        let mut alpha = passthrough_child();
        alpha.name = "alpha".to_string();
        let mut beta = passthrough_child();
        beta.name = "beta".to_string();
        let caps = mock_capabilities_with_resolver(
            MockWorkflowResolver::default()
                .with("wf-alpha", alpha)
                .with("wf-beta", beta),
        );
        let input = vec![
            crate::data::Item::new(json!({ "which": "wf-alpha" })),
            crate::data::Item::new(json!({ "which": "wf-beta" })),
        ];
        let out = execute_over(
            json!({ "workflow_id": "=item.which", "execution": "per_item" }),
            input,
            &caps,
        )
        .await;
        assert_eq!(out.items.len(), 2);
        // Both resolved (an unknown id would have errored the batch), and each
        // child echoed its own seed.
        assert_eq!(
            out.items[0].json["run"]["trigger"][0]["json"]["which"],
            "wf-alpha"
        );
        assert_eq!(
            out.items[1].json["run"]["trigger"][0]["json"]["which"],
            "wf-beta"
        );
    }

    #[tokio::test]
    async fn a_fanned_out_child_failure_is_collected_not_fatal() {
        // Only `wf-ok` resolves; the other item's child fails to resolve. Under
        // a fan-out's collect default the batch still returns one item per
        // input, with the failure marked for a downstream branch.
        let caps = mock_capabilities_with_resolver(
            MockWorkflowResolver::default().with("wf-ok", passthrough_child()),
        );
        let input = vec![
            crate::data::Item::new(json!({ "which": "wf-ok" })),
            crate::data::Item::new(json!({ "which": "wf-missing" })),
        ];
        let out = execute_over(
            json!({ "workflow_id": "=item.which", "execution": "per_item", "concurrency": 2 }),
            input,
            &caps,
        )
        .await;

        assert_eq!(out.items.len(), 2, "one output per input even on failure");
        assert!(
            out.items[0].json["nodes"]["ct"].is_object(),
            "the good child ran"
        );
        assert_eq!(out.items[1].json["json"]["failed"], true);
        assert!(
            out.items[1].json["json"]["error"]
                .as_str()
                .expect("error message")
                .contains("wf-missing")
        );
    }

    #[tokio::test]
    async fn a_fan_out_widens_the_run_without_deepening_it() {
        // Every sibling child runs at depth+1; a fan-out of N must not consume N
        // levels of the nesting budget (which would make wide fan-outs of nested
        // workflows spuriously trip the cycle guard).
        let caps = mock_capabilities_with_resolver(
            MockWorkflowResolver::default().with("child-1", passthrough_child()),
        );
        let input: Vec<_> = (0..12)
            .map(|i| crate::data::Item::new(json!({ "i": i })))
            .collect();
        let out = execute_over(
            json!({ "workflow_id": "child-1", "execution": "per_item", "concurrency": "all" }),
            input,
            &caps,
        )
        .await;
        assert_eq!(out.items.len(), 12, "12 siblings all completed");
        for item in &out.items {
            assert_eq!(
                item.json["run"]["sub_workflow_depth"], 1,
                "every sibling runs one level down, not cumulatively deeper"
            );
        }
    }

    #[tokio::test]
    async fn missing_workflow_config_is_a_capability_error() {
        let err = execute_err(Value::Null).await;
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("workflow")),
            "expected a capability error mentioning `workflow`, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn invalid_workflow_value_is_a_capability_error() {
        // A non-graph value under `workflow` fails to deserialize into a graph.
        let err = execute_err(json!({ "workflow": 123 })).await;
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("invalid workflow")),
            "expected a capability error about an invalid workflow, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn sub_workflow_runs_embedded_child_graph() {
        // The child is a single trigger node; serialize it into the parent's
        // sub_workflow config so the executor compiles and runs it.
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger)],
            ..Default::default()
        };
        let child_value = serde_json::to_value(&child).expect("serialize child");

        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = json!({ "workflow": child_value });

        let parent = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), sw],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "sw".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };

        let compiled = compile(&parent).expect("compile parent");
        let caps = mock_capabilities();

        let out = run(&compiled, json!({ "hi": 1 }), &caps)
            .await
            .expect("run parent");

        // The sub_workflow emits the child's final run state as its single item.
        // The child seeds its trigger from the input the parent passed, which is
        // the serialized parent items delivered to the sub_workflow node — an
        // array of `Item`s — so the child's `run.trigger` is that array.
        let child_state = &out.output["nodes"]["sw"]["items"][0]["json"];
        assert_eq!(
            child_state["run"]["trigger"],
            json!([{ "json": { "hi": 1 } }]),
            "child trigger should be seeded with the parent's serialized items"
        );
        // And the child actually ran: its trigger node recorded that same payload.
        assert_eq!(
            child_state["nodes"]["ct"]["items"][0]["json"],
            json!([{ "json": { "hi": 1 } }]),
            "child trigger node should have run and echoed its seeded input"
        );
    }

    /// Executes a lone `sub_workflow` node with the given config, run metadata,
    /// and capabilities, returning its raw [`Result`].
    async fn execute_with(
        config: Value,
        run_meta: Value,
        caps: &Capabilities,
    ) -> Result<NodeOutput, EngineError> {
        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = config;
        let input = vec![];
        let ctx = NodeContext {
            node: &sw,
            input: &input,
            run: &run_meta,
            nodes: &Value::Null,
            caps,
        };
        SubWorkflowNode.execute(ctx).await
    }

    #[tokio::test]
    async fn both_workflow_and_workflow_id_is_rejected() {
        // Exactly one of the two config keys may be set.
        let err = execute_err(json!({
            "workflow": { "nodes": [], "edges": [] },
            "workflow_id": "child-1"
        }))
        .await;
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("exactly one")),
            "expected an exactly-one config error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn empty_workflow_id_falls_back_to_missing_config_error() {
        // A blank `workflow_id` is treated as absent, so with no inline
        // `workflow` either the node reports the missing-config error.
        let err = execute_err(json!({ "workflow_id": "" })).await;
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("missing")),
            "expected a missing-config error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn sub_workflow_by_id_resolves_via_resolver_and_executes() {
        // The saved child is a single trigger node, registered under an id the
        // parent references via `workflow_id`.
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger)],
            ..Default::default()
        };
        let caps =
            mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = json!({ "workflow_id": "child-1" });
        let parent = WorkflowGraph {
            nodes: vec![node("t", NodeKind::Trigger), sw],
            edges: vec![Edge {
                from_node: "t".to_string(),
                from_port: "main".to_string(),
                to_node: "sw".to_string(),
                to_port: "main".to_string(),
            }],
            ..Default::default()
        };
        let compiled = compile(&parent).expect("compile parent");

        let out = run(&compiled, json!({ "hi": 1 }), &caps)
            .await
            .expect("run parent");

        // The referenced child was resolved and actually ran.
        let child_state = &out.output["nodes"]["sw"]["items"][0]["json"];
        assert_eq!(
            child_state["nodes"]["ct"]["items"][0]["json"],
            json!([{ "json": { "hi": 1 } }]),
            "resolved child trigger node should have run and echoed its seeded input"
        );
        // The child ran one nesting level deep.
        assert_eq!(child_state["run"]["sub_workflow_depth"], json!(1));
    }

    #[tokio::test]
    async fn unknown_workflow_id_surfaces_resolver_error() {
        // The default mock resolver knows no ids, so resolution fails.
        let caps = mock_capabilities();
        let err = execute_with(json!({ "workflow_id": "nope" }), Value::Null, &caps)
            .await
            .expect_err("unknown id must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("nope")),
            "expected the resolver's unknown-id error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn direct_self_reference_by_id_is_rejected() {
        // The saved child itself references the same id — a one-level cycle,
        // caught statically before it runs.
        let mut inner = node("inner", NodeKind::SubWorkflow);
        inner.config = json!({ "workflow_id": "loop-1" });
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger), inner],
            ..Default::default()
        };
        let caps =
            mock_capabilities_with_resolver(MockWorkflowResolver::default().with("loop-1", child));

        let err = execute_with(json!({ "workflow_id": "loop-1" }), Value::Null, &caps)
            .await
            .expect_err("self-reference must be rejected");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("cycle")),
            "expected a cycle rejection, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn depth_limit_is_enforced() {
        // A run already at the maximum nesting depth refuses to descend further,
        // even for a trivial resolvable child (bounds indirect cycles).
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger)],
            ..Default::default()
        };
        let caps =
            mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

        let run_meta = json!({ "sub_workflow_depth": crate::engine::MAX_SUB_WORKFLOW_DEPTH });
        let err = execute_with(json!({ "workflow_id": "child-1" }), run_meta, &caps)
            .await
            .expect_err("exceeding the depth budget must error");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("depth")),
            "expected a depth-limit error, got: {err:?}"
        );
    }
}
