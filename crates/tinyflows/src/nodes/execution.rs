use super::*;

/// How a capability node maps over its input items.
///
/// - [`Once`](ExecutionMode::Once): run a single time, binding config against
///   the first input item — one output item regardless of input count.
/// - [`PerItem`](ExecutionMode::PerItem): map over the input, re-resolving
///   config against each item and emitting one output item per input (carrying
///   `paired_item`). This is the n8n-style default for `tool_call` /
///   `http_request`, so a fan-out (`split_out` → node) actually runs per element
///   instead of silently dropping all but the first.
///
/// `PerItem` says *that* the node maps over its input; [`map`] says **how many
/// items run at a time** (`config.concurrency`) and what a failing item does to
/// the batch (`config.on_item_error`). Concurrency defaults to `1`, so a node
/// that does not opt in keeps the sequential ordering and timing it always had.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecutionMode {
    /// Single invocation against the first item.
    Once,
    /// One invocation per input item.
    PerItem,
}

/// Reads the node's `execution` config (`"once"` | `"per_item"`), falling back
/// to `default` when unset or unrecognized. Read from the raw config (not an
/// `=`-expression) since it selects the resolution strategy itself.
#[must_use]
pub(crate) fn execution_mode(config: &Value, default: ExecutionMode) -> ExecutionMode {
    match config.get("execution").and_then(Value::as_str) {
        Some("per_item") => ExecutionMode::PerItem,
        Some("once") => ExecutionMode::Once,
        _ => default,
    }
}

/// Projects the run state's `nodes` map into the expression-scope shape:
/// `{ "<id>": { "item": <first item json>, "items": [<item json>…] } }`.
///
/// Each state slot stores serialized [`Item`]s (`{ "json": …, … }`); the scope
/// exposes just the `json` payloads, mirroring how `item`/`items` are projected
/// from the node's own input. Slots without an `items` array (or a non-object
/// `nodes` value) are skipped, so an absent run state yields `{}`.
pub(crate) fn nodes_scope(nodes: &Value) -> Value {
    let mut scope = serde_json::Map::new();
    if let Value::Object(map) = nodes {
        for (id, slot) in map {
            let Some(items) = slot.get("items").and_then(Value::as_array) else {
                continue;
            };
            let jsons: Vec<Value> = items
                .iter()
                .map(|item| item.get("json").cloned().unwrap_or(Value::Null))
                .collect();
            let first = jsons.first().cloned().unwrap_or(Value::Null);
            let mut entry = serde_json::json!({ "item": first, "items": jsons });
            // Slot state a node recorded about itself via `NodeOutput::meta`,
            // promoted so expressions can read it the same way they read items.
            // This is what makes `=nodes.<loop id>.iteration` and
            // `=nodes.<loop id>.state` resolve from anywhere in the graph.
            //
            // Promoted by exclusion rather than by an allow-list: `items` and
            // `port` are the slot's own structure and are already projected
            // above, and `_`-prefixed keys are engine bookkeeping. Everything
            // else is something a node chose to record about itself, and a list
            // naming each one would need a line per new meta key.
            for (key, value) in slot.as_object().into_iter().flatten() {
                if key == "items" || key == "port" || key.starts_with('_') {
                    continue;
                }
                entry[key.as_str()] = value.clone();
            }
            scope.insert(id.clone(), entry);
        }
    }
    Value::Object(scope)
}

/// Resolves a node's config against its expression scope, tracing and logging
/// every `=`-expression that resolved to `null`.
///
/// The shared data-binding entry point for capability-backed nodes: the
/// resolved config is identical to `expr::resolve`'s, and each null-resolved
/// expression is `tracing::warn!`ed with the node id, config location, and the
/// original expression, then returned so the node can attach it to its
/// [`NodeOutput::diagnostics`]. Diagnostics are non-fatal by design — a null
/// may be intended, and failure policy belongs to routing/`on_error`.
pub(crate) fn resolve_config_traced(
    ctx: &NodeContext,
) -> (Value, Vec<crate::expr::NullResolution>) {
    resolve_config_traced_with_scope(ctx, expr_scope(ctx))
}

/// Like [`resolve_config_traced`], but binds `item` to `item_json` (the current
/// item in a per-item run) instead of the first input item.
pub(crate) fn resolve_config_traced_for_item(
    ctx: &NodeContext,
    item_json: Value,
) -> (Value, Vec<crate::expr::NullResolution>) {
    resolve_config_traced_with_scope(ctx, expr_scope_for(ctx, item_json))
}

/// Shared body: resolve the node config against a pre-built `scope`, warning on
/// each null-resolved `=`-expression.
fn resolve_config_traced_with_scope(
    ctx: &NodeContext,
    scope: Value,
) -> (Value, Vec<crate::expr::NullResolution>) {
    let (cfg, misses) = crate::expr::resolve_traced(&ctx.node.config, &scope);
    for miss in &misses {
        tracing::warn!(
            node = %ctx.node.id,
            location = %miss.location,
            expression = %miss.expression,
            "config expression resolved to null; check the wiring (`nodes.<id>.item.<field>`)"
        );
    }
    (cfg, misses)
}

/// The outcome of executing a single node: the items it emits and (for branching
/// nodes) which output port to follow.
#[derive(Debug, Clone, Default)]
pub struct NodeOutput {
    /// The items this node emits. A node maps over its input and returns an array
    /// of output [`Item`]s (which may be empty).
    pub items: Vec<Item>,
    /// For branching nodes, the output port to follow (e.g. `"true"`); `None`
    /// means the default `"main"` port.
    pub port: Option<String>,
    /// Non-fatal data-binding diagnostics: every config `=`-expression that
    /// resolved to `null` during this execution (see
    /// [`crate::expr::resolve_traced`]). Surfaced on the run's
    /// [`ExecutionStep`](crate::observability::ExecutionStep) so a host can
    /// point at the exact unresolved wiring; failure policy stays with
    /// routing/`on_error`.
    pub diagnostics: Vec<crate::expr::NullResolution>,
    /// Extra keys to record alongside `items`/`port` in this node's run-state
    /// slot, for a node that must remember something across its own
    /// activations. `None` writes nothing.
    ///
    /// The only current user is [`NodeKind::Loop`](crate::model::NodeKind::Loop),
    /// which keeps its `iteration` count here. Slot state is the right home for
    /// it because the run state is what gets checkpointed, so an iteration
    /// count rides resume for free and is addressable from expressions as
    /// `=nodes.<id>.iteration` — whereas a counter held in the executor would
    /// be lost the moment the run paused, and a counter threaded through items
    /// would be destroyed by any node in the body that reshapes them.
    ///
    /// Must be a JSON object; anything else is ignored when the slot is built.
    pub meta: Option<Value>,
    /// A request to the engine for something other than "emit and move on".
    /// `None` — the overwhelmingly common case — means ordinary data flow.
    ///
    /// This is the channel that lets an executor return *control* rather than
    /// only data. Without it a node can express "here are my items" and "I
    /// failed", but not "pause the run here" or "ask me again shortly" — which
    /// is why, before this existed, a `sub_workflow` whose child paused at an
    /// approval gate had to fail the parent outright rather than pause it.
    pub control: Option<NodeControl>,
    /// What a harness did inside this node, in order.
    ///
    /// Only an `agent` node fills this in, and only when the host's
    /// [`AgentRunner`](crate::caps::AgentRunner) reported one on its
    /// [`AgentRunOutcome`](crate::caps::AgentRunOutcome). The engine copies it
    /// onto the node's
    /// [`ExecutionStep`](crate::observability::ExecutionStep) and otherwise
    /// does not read it — it is carried, never interpreted.
    ///
    /// Empty for every other node kind, and for a harness with no event stream
    /// to fold.
    pub transcript: Vec<crate::transcript::TranscriptEntry>,
}

/// What a node asks the engine to do instead of simply emitting its items.
///
/// Both variants are lowered in [`crate::engine`]'s node handler, which is the
/// only place that can speak to the underlying super-step executor.
#[derive(Debug, Clone, PartialEq)]
pub enum NodeControl {
    /// Pause the whole run here, surfacing `id` on the run's pending set.
    ///
    /// The engine turns this into a `tinyagents` interrupt, which **discards
    /// this activation's state update** — the node re-runs from the top when
    /// the run is resumed. An executor using this must therefore be safe to
    /// re-enter: record what it needs to recognise the resume (a ticket, a
    /// child thread id) in its slot on an *earlier* activation, not this one.
    Interrupt {
        /// Identifies the pause to the host, and addresses the resume value
        /// back to this node. Conventionally the node id.
        id: String,
        /// Host-facing description of what is being waited on.
        payload: Value,
    },
    /// Re-activate this same node in the next super-step, after a delay.
    ///
    /// State-driven waiting: the node's update *is* committed, so it can leave
    /// itself notes, and it is then re-run to look at the world again. This is
    /// how a gather or gate waits for lanes and tickets that settle in
    /// different super-steps.
    ///
    /// Each poll costs one super-step and one node visit against the run's
    /// budgets, so **every** user of this must carry its own bounded poll count
    /// rather than relying on the run-level backstop to stop it.
    Reenter {
        /// How long to wait before the next activation, in milliseconds.
        after_ms: u64,
    },
    /// Fan this node's successors out into `lanes` **parallel copies**, one per
    /// entry, each carrying its own slice of the work.
    ///
    /// The difference from an ordinary fan-out is what gets duplicated. A
    /// fan-out runs each *successor* once; a scatter runs the *whole downstream
    /// path* once per lane, so a five-node pipeline becomes N concurrent
    /// five-node pipelines. That is only expressible as a routing decision —
    /// the engine schedules one activation per (lane × successor), each with its
    /// own input — so it comes back through this channel rather than as items.
    Scatter {
        /// The work for each lane, in lane order. Emission downstream is
        /// ordered by this index rather than by completion, so a run is
        /// reproducible whatever the timing.
        lanes: Vec<Vec<Item>>,
    },
}

impl NodeOutput {
    /// Builds an output on the default `"main"` port.
    #[must_use]
    pub fn main(items: Vec<Item>) -> Self {
        Self {
            items,
            ..Self::default()
        }
    }

    /// Builds an output that routes to the named `port`.
    #[must_use]
    pub fn routed(items: Vec<Item>, port: impl Into<String>) -> Self {
        Self {
            items,
            port: Some(port.into()),
            ..Self::default()
        }
    }

    /// Builds an empty output on the default port (a node that produced no items).
    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    /// The same output, carrying what a harness did to produce it.
    ///
    /// See [`transcript`](Self::transcript). Only the `agent` node uses this.
    #[must_use]
    pub fn with_transcript(mut self, transcript: Vec<crate::transcript::TranscriptEntry>) -> Self {
        self.transcript = transcript;
        self
    }

    /// Attaches data-binding diagnostics (null-resolved expressions) to this
    /// output.
    #[must_use]
    pub fn with_diagnostics(mut self, diagnostics: Vec<crate::expr::NullResolution>) -> Self {
        self.diagnostics = diagnostics;
        self
    }

    /// Attaches extra run-state slot keys to this output (see
    /// [`NodeOutput::meta`]).
    #[must_use]
    pub fn with_meta(mut self, meta: Value) -> Self {
        self.meta = Some(meta);
        self
    }

    /// Attaches a control request to this output (see [`NodeOutput::control`]).
    #[must_use]
    pub fn with_control(mut self, control: NodeControl) -> Self {
        self.control = Some(control);
        self
    }

    /// Builds an output that pauses the run, surfacing `id` on its pending set.
    ///
    /// Carries no items: an interrupt discards this activation's update, so
    /// anything set here would be thrown away.
    #[must_use]
    pub fn interrupt(id: impl Into<String>, payload: Value) -> Self {
        Self::empty().with_control(NodeControl::Interrupt {
            id: id.into(),
            payload,
        })
    }

    /// Builds an output that commits `meta` and asks to be re-run after
    /// `after_ms` milliseconds.
    #[must_use]
    pub fn reenter_after(after_ms: u64, meta: Value) -> Self {
        Self::empty()
            .with_meta(meta)
            .with_control(NodeControl::Reenter { after_ms })
    }

    /// Builds an output that fans the downstream path out into parallel lanes.
    #[must_use]
    pub fn scatter(lanes: Vec<Vec<Item>>, meta: Value) -> Self {
        Self::empty()
            .with_meta(meta)
            .with_control(NodeControl::Scatter { lanes })
    }
}

/// Executes one node kind.
#[async_trait]
pub trait NodeExecutor: Send + Sync {
    /// Runs the node and returns its output (or a routing decision).
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput>;
}

/// A trigger node's executor: it echoes its input items through unchanged. The
/// engine seeds the trigger payload directly into the run state, so at runtime
/// the trigger is a passthrough; this executor makes the dispatch table total.
#[derive(Debug, Default, Clone)]
struct TriggerNode;

#[async_trait]
impl NodeExecutor for TriggerNode {
    async fn execute(&self, ctx: NodeContext<'_>) -> Result<NodeOutput> {
        Ok(NodeOutput::main(ctx.input.to_vec()))
    }
}

/// Returns the [`NodeExecutor`] for a given [`NodeKind`].
///
/// Native control-flow executors live in [`control_flow`]; capability-backed
/// ones in [`integration`]. The engine uses this to dispatch each graph node.
#[must_use]
pub(crate) fn executor_for(kind: &NodeKind) -> Box<dyn NodeExecutor> {
    match kind {
        NodeKind::Trigger => Box::new(TriggerNode),
        NodeKind::Agent => Box::new(integration::AgentNode),
        NodeKind::ToolCall => Box::new(integration::ToolCallNode),
        NodeKind::HttpRequest => Box::new(integration::HttpRequestNode),
        NodeKind::Code => Box::new(integration::CodeNode),
        NodeKind::Shell => Box::new(integration::ShellNode),
        NodeKind::OutputParser => Box::new(integration::OutputParserNode),
        NodeKind::SubWorkflow => Box::new(integration::SubWorkflowNode),
        NodeKind::Memory => Box::new(integration::MemoryNode),
        NodeKind::Condition => Box::new(control_flow::ConditionNode),
        NodeKind::Switch => Box::new(control_flow::SwitchNode),
        NodeKind::Merge => Box::new(control_flow::MergeNode),
        NodeKind::SplitOut => Box::new(control_flow::SplitOutNode),
        NodeKind::Transform => Box::new(control_flow::TransformNode),
        NodeKind::Dedup => Box::new(control_flow::DedupNode),
        NodeKind::Void => Box::new(control_flow::VoidNode),
        NodeKind::Scatter => Box::new(control_flow::ScatterNode),
        NodeKind::Gather => Box::new(control_flow::GatherNode),
        NodeKind::Spawn => Box::new(integration::SpawnNode),
        NodeKind::Gate => Box::new(integration::GateNode),
        NodeKind::Approval => Box::new(integration::ApprovalNode),
        NodeKind::Loop => Box::new(control_flow::LoopNode),
    }
}
