use super::*;

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
            let opts = crate::nodes::map::map_options(&ctx.node.config, &ctx.node.id, ctx.run);
            let ctx = &ctx;
            let (items, _) = crate::nodes::map::map_items(
                ctx.input.len(),
                &ctx.node.id,
                ctx.observer,
                opts,
                move |index| async move {
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
                    // A paused child is reported as a marker item rather than
                    // an error: the map slots exactly one output per input
                    // index, and the gates are collected across the whole batch
                    // below so one pause does not hide the others.
                    let child = match run_child(ctx, &scope, std::slice::from_ref(item)).await? {
                        ChildOutcome::Finished(item) => item,
                        ChildOutcome::Cancelled => crate::data::Item::new(Value::Null),
                        ChildOutcome::Paused(gates) => {
                            crate::data::Item::new(json!({ PAUSED_MARKER: gates }))
                        }
                    };
                    Ok((child, vec![]))
                },
            )
            .await?;
            // Parent-initiated cancel: wind down with no output, mirroring the
            // top-level cancelled-node contract. `ctx.token` is a one-way flag,
            // so if any child wound down (returned `None`) it is set here; the
            // parent's next boundary check sees the same flip and settles
            // `cancelled = true`.
            if ctx.token.is_cancelled() {
                return Ok(NodeOutput::empty());
            }
            // Any child that paused pauses the whole node. Gates from every
            // paused child are unioned, so a host sees all of them at once
            // rather than discovering them one fan-out element at a time.
            let paused: Vec<String> = items
                .iter()
                .filter_map(|item| item.json.get(PAUSED_MARKER))
                .filter_map(Value::as_array)
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            if !paused.is_empty() {
                return Ok(pause_for_child_gates(&ctx.node.id, paused));
            }
            return Ok(NodeOutput::main(items));
        }

        let scope = crate::nodes::expr_scope(&ctx);
        match run_child(&ctx, &scope, ctx.input).await? {
            ChildOutcome::Finished(item) => Ok(NodeOutput::main(vec![item])),
            // Parent-initiated cancel wound the child down: emit nothing, the
            // same clean wind-down a top-level cancelled node performs. The
            // parent's next boundary check settles `cancelled = true`.
            ChildOutcome::Cancelled => Ok(NodeOutput::empty()),
            ChildOutcome::Paused(gates) => Ok(pause_for_child_gates(&ctx.node.id, gates)),
        }
    }
}

/// Marks a per-item child result that paused rather than finished.
///
/// Underscore-prefixed so it cannot collide with a child's own output keys. It
/// never reaches downstream: the fan-out collects these, pauses the node, and
/// the interrupt discards the items.
const PAUSED_MARKER: &str = "_sub_workflow_paused";

/// Builds the parent-side pause for a child that stopped at approval gates.
///
/// The interrupt id is the first gate, because a run's pending set is keyed by
/// interrupt id and a node emits one interrupt. The payload carries the full
/// list, and a host may approve several at once — the next re-run seeds all of
/// them into the child, so a child with N gates need not cost N round trips.
fn pause_for_child_gates(node_id: &str, gates: Vec<String>) -> NodeOutput {
    tracing::info!(
        node = %node_id,
        ?gates,
        "sub_workflow: child paused awaiting approval; pausing the parent"
    );
    let first = gates
        .first()
        .cloned()
        .unwrap_or_else(|| node_id.to_string());
    NodeOutput::interrupt(
        first,
        json!({
            "kind": "sub_workflow_approval",
            "node": node_id,
            "pending": gates,
        }),
    )
}

/// The workspace the child run is pinned to: this node's `workspace` override
/// when it declares one, else the parent's.
///
/// The override is resolved against the parent's workspace and refused if it
/// escapes it — the same rule, and the same code, as an `agent` node's `cwd`
/// (see [`crate::workdir`]). A parent run with no workspace has nothing to
/// contain the value against, so it is taken as written: that is how a graph
/// declares a workspace for a child when the run itself was never pinned to one.
async fn child_workspace(ctx: &NodeContext<'_>, scope: &Value) -> Result<Option<String>> {
    let Some(declared) = ctx.node.config.get("workspace").filter(|v| !v.is_null()) else {
        return Ok(crate::workdir::run_workspace(ctx.run).map(str::to_string));
    };
    let declared = crate::expr::resolve(declared, scope);
    let raw = declared.as_str().ok_or_else(|| {
        EngineError::Capability(format!(
            "sub_workflow node {}: `workspace` must be a string",
            ctx.node.id
        ))
    })?;
    if raw.trim().is_empty() {
        return Err(EngineError::Capability(format!(
            "sub_workflow node {}: `workspace` must be a non-empty path when present",
            ctx.node.id
        )));
    }
    Ok(Some(
        crate::workdir::resolve_node_dir(
            ctx.caps.agent.as_ref(),
            ctx.run,
            raw,
            "config.workspace",
            &format!("sub_workflow node {}", ctx.node.id),
        )
        .await?,
    ))
}

/// What one child run produced, from the parent node's point of view.
///
/// Three outcomes rather than two: a child can finish, wind down because the
/// parent cancelled, or **pause** at an approval gate. The last one is not a
/// failure and not a result — it has to travel up as its own case so the parent
/// node can pause too, rather than being flattened into an item or an error.
enum ChildOutcome {
    /// The child ran to completion; its final state is this item.
    Finished(crate::data::Item),
    /// The parent cancelled mid-child. A clean cooperative wind-down.
    Cancelled,
    /// The child stopped at one or more approval gates, named here with the
    /// parent-facing namespace already applied.
    Paused(Vec<String>),
}

/// Resolves this node's child graph and runs it once, returning the child's
/// final run state as a single [`Item`](crate::data::Item).
///
/// `scope` is the expression scope `workflow_id` is resolved against (the whole
/// input for `once`, the current element for `per_item`), and `child_input` is
/// the item array seeded into the child run.
///
/// Returns [`ChildOutcome::Cancelled`] when the parent run cancelled this child mid-flight
/// (`ctx.token` is set): the child is a clean cooperative wind-down, not a
/// failure, so it emits no item and lets the parent settle as cancelled. A child
/// that stops for any *other* reason (a `requires_approval` pause, or a cancel
/// arriving through a channel independent of the parent's token) still errors.
async fn run_child(
    ctx: &NodeContext<'_>,
    scope: &Value,
    child_input: &[crate::data::Item],
) -> Result<ChildOutcome> {
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
    // Approvals the parent has accumulated for *this* node's child gates. Empty
    // on a first run; populated after a resume, which is what lets the re-run
    // get past the gate that paused it.
    //
    // Delivered through `RunInput::with_approvals` rather than written into the
    // trigger payload: a child is seeded with its input *items*, so its trigger
    // is an array and has nowhere to carry an `approvals` key.
    let child_approvals = approvals_for_child(ctx);
    // Resolved against the same `scope` as `workflow_id`, so a `per_item` run
    // forwards values derived from *its* element (`"=item.repo"`) rather than
    // from the batch — the whole point of resolving inputs in here rather than
    // once at the call site.
    let child_inputs = child_inputs(&ctx.node.config, scope)?;
    // Where the child runs. `config.workspace` (expression-resolved like
    // `workflow_id`, so it can name a directory an earlier node created) becomes
    // the child run's workspace, held to the same containment rule as an
    // `agent` node's `cwd`: it must resolve inside the parent's workspace. With
    // no override the child inherits the parent's.
    let child_workspace = child_workspace(ctx, scope).await?;
    // Box the recursive engine call so the async future type stays sized.
    // Forward the parent run's cancellation token: cancelling the parent must
    // wind down this child too, rather than letting it run on orphaned behind a
    // fresh token. The child threads it into its own node contexts, so the whole
    // nesting chain shares one cancellation signal.
    let outcome = Box::pin(crate::engine::run_sub_workflow(
        &compiled,
        crate::engine::RunInput::new(trigger)
            .with_inputs(child_inputs)
            .with_approvals(child_approvals),
        ctx.caps,
        child_depth,
        depth_cap,
        ctx.token.clone(),
        child_workspace,
    ))
    .await?;

    // Enforce the child's lifecycle across the sub-workflow boundary (BUG-5).
    //
    // The child run is a *separate* engine invocation whose non-completion is
    // reported on its [`RunOutcome`], not on the [`NodeOutput`] this node
    // returns. What must never happen is keeping only `outcome.output` and
    // reporting success — that would silently treat a child paused at a
    // `requires_approval` gate as if it had run to completion, making approval
    // gating unenforceable across the boundary.
    //
    // A paused child now **pauses the parent** rather than failing it, via
    // [`NodeControl::Interrupt`]. The child's gate ids are namespaced by this
    // node's id (`<node>::<child gate>`) so they cannot collide with the
    // parent's own gates, and so this node can recognise its own approvals when
    // it re-runs.
    //
    // Resume works the way `engine::resume` already works everywhere else: by
    // re-executing with the merged approval set rather than replaying a
    // checkpoint. The approvals accumulate in the parent's
    // `run.trigger.approvals`; on the re-run this node reads back the ones
    // addressed to it, strips the namespace, and seeds them into the child's
    // trigger — so the child gets past the gate that stopped it. Nothing needs
    // to share a checkpointer, and the child is deterministic, so re-running it
    // reaches the same place.
    if !outcome.pending_approvals.is_empty() {
        let namespaced: Vec<String> = outcome
            .pending_approvals
            .iter()
            .map(|gate| namespaced_gate(&ctx.node.id, gate))
            .collect();
        return Ok(ChildOutcome::Paused(namespaced));
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
            return Ok(ChildOutcome::Cancelled);
        }
        return Err(EngineError::Capability(format!(
            "sub_workflow node {:?}: child run was cancelled before completing; the parent \
             run is halted rather than falsely completed",
            ctx.node.id
        )));
    }

    Ok(ChildOutcome::Finished(crate::data::Item::new(
        outcome.output,
    )))
}
