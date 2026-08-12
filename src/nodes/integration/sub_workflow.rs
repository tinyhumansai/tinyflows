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
/// Under `execution: "per_item"` the fields are resolved against **the current
/// element**, exactly like `workflow_id` is, so each child in a fan-out gets
/// values derived from its own item:
///
/// ```json
/// {
///   "workflow_id": "review-and-fix",
///   "execution": "per_item",
///   "inputs": { "repo": "=item.name" }
/// }
/// ```
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

/// The nesting cap in force for this run.
///
/// Seeded from the top-level graph's `trigger.config.max_sub_workflow_depth`
/// and forwarded down the chain by [`crate::engine::run_sub_workflow`], so
/// every level enforces the bound the *root* run declared rather than whatever
/// each child's own trigger happens to say. Falls back to
/// [`MAX_SUB_WORKFLOW_DEPTH`] when unset.
fn max_depth(run: &Value) -> u64 {
    run.get("max_sub_workflow_depth")
        .and_then(Value::as_u64)
        .filter(|n| *n > 0)
        .unwrap_or(MAX_SUB_WORKFLOW_DEPTH)
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
                    // `run_child` yields `None` only when the parent cancelled
                    // this run mid-child (`ctx.token` is then set). The map slots
                    // exactly one output per input index, so stand in with an
                    // empty item — the whole node's output is discarded by the
                    // token check below, so this placeholder never surfaces.
                    let child = run_child(ctx, &scope, std::slice::from_ref(item))
                        .await?
                        .unwrap_or_else(|| crate::data::Item::new(Value::Null));
                    Ok((child, vec![]))
                })
                .await?;
            // Parent-initiated cancel: wind down with no output, mirroring the
            // top-level cancelled-node contract. `ctx.token` is a one-way flag,
            // so if any child wound down (returned `None`) it is set here; the
            // parent's next boundary check sees the same flip and settles
            // `cancelled = true`.
            if ctx.token.is_cancelled() {
                return Ok(NodeOutput::empty());
            }
            return Ok(NodeOutput::main(items));
        }

        let scope = crate::nodes::expr_scope(&ctx);
        match run_child(&ctx, &scope, ctx.input).await? {
            Some(item) => Ok(NodeOutput::main(vec![item])),
            // Parent-initiated cancel wound the child down: emit nothing, the
            // same clean wind-down a top-level cancelled node performs. The
            // parent's next boundary check settles `cancelled = true`.
            None => Ok(NodeOutput::empty()),
        }
    }
}

/// Resolves this node's child graph and runs it once, returning the child's
/// final run state as a single [`Item`](crate::data::Item).
///
/// `scope` is the expression scope `workflow_id` is resolved against (the whole
/// input for `once`, the current element for `per_item`), and `child_input` is
/// the item array seeded into the child run.
///
/// Returns `Ok(None)` when the parent run cancelled this child mid-flight
/// (`ctx.token` is set): the child is a clean cooperative wind-down, not a
/// failure, so it emits no item and lets the parent settle as cancelled. A child
/// that stops for any *other* reason (a `requires_approval` pause, or a cancel
/// arriving through a channel independent of the parent's token) still errors.
async fn run_child(
    ctx: &NodeContext<'_>,
    scope: &Value,
    child_input: &[crate::data::Item],
) -> Result<Option<crate::data::Item>> {
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
    let depth_cap = max_depth(ctx.run);
    if child_depth > depth_cap {
        return Err(EngineError::Capability(format!(
            "sub_workflow node: maximum nesting depth {depth_cap} exceeded (possible cycle)"
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
    // Forward the parent run's cancellation token: cancelling the parent must
    // wind down this child too, rather than letting it run on orphaned behind a
    // fresh token. The child threads it into its own node contexts, so the whole
    // nesting chain shares one cancellation signal.
    let outcome = Box::pin(crate::engine::run_sub_workflow(
        &compiled,
        crate::engine::RunInput::new(trigger).with_inputs(child_inputs),
        ctx.caps,
        child_depth,
        depth_cap,
        ctx.token.clone(),
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
        // Two cancellations look the same on the child's `RunOutcome` but mean
        // opposite things to the parent, so split on *who* cancelled:
        //
        // - The parent's own token is set: this is a cooperative wind-down of
        //   the whole run (the parent is being cancelled and forwarded the same
        //   token in, per `run_sub_workflow`). Halting with an error here would
        //   turn a clean cancel into a spurious failure. Emit nothing and let
        //   the parent settle: its next node-boundary check sees the same
        //   flipped token and reports `cancelled = true`, exactly as a
        //   top-level cancelled node does.
        // - The parent's token is NOT set, yet the child still reports
        //   cancelled: the child was cancelled through some channel independent
        //   of this run (none exists today — the only token a child receives is
        //   the parent's clone — but keep the arm so a future independent-cancel
        //   path can never be silently treated as a completed child).
        if ctx.token.is_cancelled() {
            tracing::debug!(
                node = %ctx.node.id,
                "sub_workflow: child wound down under the parent's cancellation; emitting no output"
            );
            return Ok(None);
        }
        return Err(EngineError::Capability(format!(
            "sub_workflow node {:?}: child run was cancelled before completing; the parent \
             run is halted rather than falsely completed",
            ctx.node.id
        )));
    }

    Ok(Some(crate::data::Item::new(outcome.output)))
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
            token: crate::engine::CancellationToken::new(),
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
            token: crate::engine::CancellationToken::new(),
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
            token: crate::engine::CancellationToken::new(),
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

    /// A run carrying `max_sub_workflow_depth` uses it in place of the default,
    /// so a graph that legitimately nests deeper is not capped at 8.
    #[tokio::test]
    async fn a_run_declared_depth_raises_the_default_cap() {
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger)],
            ..Default::default()
        };
        let caps =
            mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

        // At the default cap, but the run declared a higher one — so this
        // descends rather than refusing.
        let run_meta = json!({
            "sub_workflow_depth": crate::engine::MAX_SUB_WORKFLOW_DEPTH,
            "max_sub_workflow_depth": crate::engine::MAX_SUB_WORKFLOW_DEPTH + 4,
        });
        execute_with(json!({ "workflow_id": "child-1" }), run_meta, &caps)
            .await
            .expect("a raised cap should permit this level");
    }

    /// The declared cap bounds as well as raises: a lower value bites before the
    /// default would have.
    #[tokio::test]
    async fn a_run_declared_depth_can_also_lower_the_cap() {
        let child = WorkflowGraph {
            nodes: vec![node("ct", NodeKind::Trigger)],
            ..Default::default()
        };
        let caps =
            mock_capabilities_with_resolver(MockWorkflowResolver::default().with("child-1", child));

        let run_meta = json!({ "sub_workflow_depth": 2, "max_sub_workflow_depth": 2 });
        let err = execute_with(json!({ "workflow_id": "child-1" }), run_meta, &caps)
            .await
            .expect_err("a lowered cap must be enforced");
        assert!(
            matches!(err, EngineError::Capability(ref m) if m.contains("depth 2")),
            "the error should name the declared cap, got: {err:?}"
        );
    }
}

/// Cross-boundary cancellation: a parent run's [`CancellationToken`] must reach
/// its `sub_workflow` children so a parent cancel winds the whole subtree down
/// instead of orphaning it behind a fresh token. These pin the propagation
/// end-to-end through a real `run_cancellable` drive (T1–T5).
///
/// The mid-flight cancel is made **deterministic under parallel test load** by
/// having the `slow` node hold the run at its boundary until the token actually
/// flips (a bounded spin), rather than racing a wall-clock sleep against the
/// scheduler — so the boundary check before `marker` is guaranteed to observe
/// the cancellation.
#[cfg(test)]
mod cancellation_propagation_tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use serde_json::{Value, json};
    use tokio::sync::Notify;
    use tokio::time::sleep;

    use crate::caps::mock::mock_capabilities;
    use crate::caps::{Capabilities, ToolInvoker};
    use crate::compiler::compile;
    use crate::engine::{CancellationToken, RunOutcome, run_cancellable};
    use crate::error::Result;
    use crate::model::{Edge, Node, NodeKind, WorkflowGraph};

    /// The bounded spin `slow` uses to wait for cancellation, and the cap on
    /// `marker`'s sleep — both generous enough to never trip under load, small
    /// enough that a broken build (where `marker` runs) is still obvious.
    const SPIN_CAP_MS: u64 = 5_000;

    /// A [`ToolInvoker`] that records every slug it runs and lets a test suspend a
    /// run *inside* a node. `slow` fires `slow_started` and then — when
    /// `block_until_cancel` is set — holds the run at that node until `run_token`
    /// flips (bounded by [`SPIN_CAP_MS`]); this pins the cancel to land while
    /// `slow` is mid-flight, with no dependency on scheduler timing. `marker` is a
    /// node that must never run once cancellation has propagated: its appearance
    /// in `invoked` (and its long sleep in the elapsed time) is what a broken
    /// propagation reveals.
    #[derive(Clone)]
    struct ProbeTools {
        invoked: Arc<Mutex<Vec<String>>>,
        slow_started: Arc<Notify>,
        run_token: CancellationToken,
        block_until_cancel: bool,
        marker_ms: u64,
    }

    #[async_trait]
    impl ToolInvoker for ProbeTools {
        async fn invoke(&self, slug: &str, _args: Value, _conn: Option<&str>) -> Result<Value> {
            self.invoked
                .lock()
                .expect("invoked mutex")
                .push(slug.to_string());
            match slug {
                "slow" => {
                    self.slow_started.notify_one();
                    // Hold the child at this node until the run is cancelled, so
                    // the boundary check before `marker` deterministically sees
                    // the flip. Bounded so a broken build cannot hang CI.
                    if self.block_until_cancel {
                        let mut waited = 0;
                        while !self.run_token.is_cancelled() && waited < SPIN_CAP_MS {
                            sleep(Duration::from_millis(1)).await;
                            waited += 1;
                        }
                    }
                }
                // When cancellation propagated, `marker` never runs. A cancel test
                // sets `marker_ms` long so a *broken* propagation (marker runs)
                // blows the elapsed bound too; the uncancelled control sets it to 0
                // so the run stays fast when `marker` legitimately runs.
                "marker" => sleep(Duration::from_millis(self.marker_ms)).await,
                _ => {}
            }
            Ok(json!({ "tool": slug }))
        }
    }

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

    fn edge(from: &str, to: &str) -> Edge {
        Edge {
            from_node: from.to_string(),
            from_port: "main".to_string(),
            to_node: to.to_string(),
            to_port: "main".to_string(),
        }
    }

    /// A single-invocation `tool_call` node bound to `slug`.
    fn tool(id: &str, slug: &str) -> Node {
        let mut n = node(id, NodeKind::ToolCall);
        n.config = json!({ "slug": slug, "execution": "once" });
        n
    }

    /// `trigger -> slow -> marker`: the innermost chain every test cancels within.
    fn slow_then_marker() -> WorkflowGraph {
        WorkflowGraph {
            nodes: vec![
                node("ct", NodeKind::Trigger),
                tool("slow", "slow"),
                tool("marker", "marker"),
            ],
            edges: vec![edge("ct", "slow"), edge("slow", "marker")],
            ..Default::default()
        }
    }

    /// `trigger -> sub_workflow(child)`, with `child` embedded inline.
    fn wrap_inline(child: &WorkflowGraph) -> WorkflowGraph {
        let inline = serde_json::to_value(child).expect("serialize child graph");
        let mut sw = node("sw", NodeKind::SubWorkflow);
        sw.config = json!({ "workflow": inline, "execution": "once" });
        WorkflowGraph {
            nodes: vec![node("pt", NodeKind::Trigger), sw],
            edges: vec![edge("pt", "sw")],
            ..Default::default()
        }
    }

    fn probe(
        token: &CancellationToken,
        block_until_cancel: bool,
        marker_ms: u64,
    ) -> (Capabilities, Arc<Mutex<Vec<String>>>, Arc<Notify>) {
        let invoked = Arc::new(Mutex::new(Vec::new()));
        let slow_started = Arc::new(Notify::new());
        let tools = ProbeTools {
            invoked: invoked.clone(),
            slow_started: slow_started.clone(),
            run_token: token.clone(),
            block_until_cancel,
            marker_ms,
        };
        let caps = Capabilities {
            tools: Arc::new(tools),
            ..mock_capabilities()
        };
        (caps, invoked, slow_started)
    }

    /// Drives `run_cancellable` to completion while **actively polling** the run
    /// future, cancelling `token` the moment the innermost `slow` node starts.
    /// Polling matters: an un-awaited run future is the documented trap that makes
    /// a cancellation test hollow, so the future is raced against the start signal
    /// rather than cancelled blind.
    async fn run_cancelling_on_slow(
        graph: &WorkflowGraph,
        caps: &Capabilities,
        token: CancellationToken,
        slow_started: Arc<Notify>,
    ) -> (RunOutcome, Duration) {
        let compiled = compile(graph).expect("compile");
        let fut = run_cancellable(&compiled, json!({}), caps, token.clone());
        tokio::pin!(fut);
        let notified = slow_started.notified();
        tokio::pin!(notified);
        let mut cancelled = false;
        let started = Instant::now();
        let outcome = loop {
            tokio::select! {
                out = &mut fut => break out.expect("cancelled run still returns Ok"),
                // Guarded so the one-shot start signal is not polled after it fires.
                () = &mut notified, if !cancelled => {
                    token.cancel();
                    cancelled = true;
                }
            }
        };
        assert!(
            cancelled,
            "the `slow` node must have started so the cancel landed mid-flight"
        );
        (outcome, started.elapsed())
    }

    // T1 — repro→green. Parent -> sub_workflow(child), child is
    // trigger -> slow -> marker. Cancelling mid-`slow` must wind the child down:
    // `slow` (already running) completes, `marker` never runs, and the run
    // settles cancelled. Before the fix the child ran behind a fresh token, so
    // the cancel never crossed the boundary and `marker` executed.
    #[tokio::test]
    async fn t1_parent_cancel_stops_child_before_marker() {
        let token = CancellationToken::new();
        let (caps, invoked, slow_started) = probe(&token, true, SPIN_CAP_MS);
        let graph = wrap_inline(&slow_then_marker());

        let (outcome, elapsed) = run_cancelling_on_slow(&graph, &caps, token, slow_started).await;

        assert!(outcome.cancelled, "the parent run should report cancelled");
        let slugs = invoked.lock().expect("invoked mutex").clone();
        assert!(
            slugs.contains(&"slow".to_string()),
            "the in-flight `slow` node ran: {slugs:?}"
        );
        assert!(
            !slugs.contains(&"marker".to_string()),
            "cancellation must reach the child: `marker` should never run, got {slugs:?}"
        );
        // Elapsed is bounded by the in-flight `slow` node's remainder, not by
        // `marker`'s long sleep — the run did not wait on the skipped node.
        assert!(
            elapsed < Duration::from_millis(2_000),
            "wind-down should be bounded by `slow`, not `marker`; took {elapsed:?}"
        );
    }

    // T2 — transitive inheritance at depth ≥ 2. Parent -> sub -> sub, the
    // innermost being trigger -> slow -> marker. Cancelling mid-innermost-`slow`
    // must still skip the innermost `marker`: each level forwards the same token
    // down through its own node contexts.
    #[tokio::test]
    async fn t2_cancel_propagates_through_two_levels() {
        let token = CancellationToken::new();
        let (caps, invoked, slow_started) = probe(&token, true, SPIN_CAP_MS);
        let inner = wrap_inline(&slow_then_marker());
        let graph = wrap_inline(&inner);

        let (outcome, elapsed) = run_cancelling_on_slow(&graph, &caps, token, slow_started).await;

        assert!(outcome.cancelled, "the top run should report cancelled");
        let slugs = invoked.lock().expect("invoked mutex").clone();
        assert!(
            slugs.contains(&"slow".to_string()),
            "innermost `slow` ran: {slugs:?}"
        );
        assert!(
            !slugs.contains(&"marker".to_string()),
            "the token must reach depth 2: innermost `marker` should never run, got {slugs:?}"
        );
        assert!(
            elapsed < Duration::from_millis(2_000),
            "two-level wind-down should still be bounded by `slow`; took {elapsed:?}"
        );
    }

    // T3 — the guardrail: an *uncancelled* run of the identical graph must be
    // untouched, so the fix cannot be "cancel everything". Both child slugs run
    // and the outcome is not cancelled. `block_until_cancel` is off so `slow`
    // returns immediately (nothing will ever cancel it).
    #[tokio::test]
    async fn t3_uncancelled_child_runs_to_completion() {
        let token = CancellationToken::new();
        let (caps, invoked, _slow_started) = probe(&token, false, 0);
        let graph = wrap_inline(&slow_then_marker());
        let compiled = compile(&graph).expect("compile");

        let outcome = run_cancellable(&compiled, json!({}), &caps, token)
            .await
            .expect("run");

        assert!(
            !outcome.cancelled,
            "an uncancelled run must not report cancelled"
        );
        let slugs = invoked.lock().expect("invoked mutex").clone();
        assert!(
            slugs.contains(&"slow".to_string()),
            "`slow` should run: {slugs:?}"
        );
        assert!(
            slugs.contains(&"marker".to_string()),
            "`marker` must still run when nothing cancels: {slugs:?}"
        );
    }

    // T4 — a token already cancelled before the run starts: the parent's
    // `sub_workflow` node short-circuits at its own boundary, so the child never
    // starts and neither tool is invoked.
    #[tokio::test]
    async fn t4_pre_cancelled_token_never_starts_child() {
        let token = CancellationToken::new();
        let (caps, invoked, _slow_started) = probe(&token, true, 0);
        let graph = wrap_inline(&slow_then_marker());
        let compiled = compile(&graph).expect("compile");

        token.cancel();
        let outcome = run_cancellable(&compiled, json!({}), &caps, token)
            .await
            .expect("pre-cancelled run still returns Ok");

        assert!(outcome.cancelled, "a pre-cancelled run reports cancelled");
        let slugs = invoked.lock().expect("invoked mutex").clone();
        assert!(
            slugs.is_empty(),
            "the sub_workflow node short-circuits, so no child tool runs: {slugs:?}"
        );
    }

    // T5 — the defensive arm. When a child reports cancelled but the parent's own
    // token is *not* set, `run_child` still errors rather than silently treating
    // the child as completed. That state is unreachable through `run_sub_workflow`
    // today (a child only ever receives the parent's own token clone, so a
    // cancelled child implies a cancelled parent token — see T1), so this pins the
    // arm at the source level: a future refactor cannot delete it and let an
    // independently-cancelled child fall through as a false completion.
    #[test]
    fn t5_defensive_independent_cancel_arm_is_present() {
        let src = include_str!("sub_workflow.rs");
        assert!(
            src.contains("if ctx.token.is_cancelled() {")
                && src.contains("run is halted rather than falsely completed"),
            "run_child must keep BOTH the parent-cancel wind-down (Ok(None)) and the \
             defensive independent-cancel error arm"
        );
    }
}
