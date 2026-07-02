# 20 — Glossary

Terms used across the tinyflows docs.

- **Workflow** — an automation, stored as a [`WorkflowGraph`](02-workflow-model.md).
- **WorkflowGraph** — the serializable definition: nodes + edges + metadata.
- **Node** — one unit of work; has a `kind`, `config`, and `ports`.
- **NodeKind** — what a node does (`trigger`, `agent`, `tool_call`, `http_request`,
  `code`, `condition`, `switch`, `merge`, `split_out`, `transform`,
  `output_parser`, `sub_workflow`).
- **Trigger / TriggerKind** — the single entry node and how it fires (manual,
  schedule, webhook, app-event, form, chat, execute-by-workflow, evaluation,
  system).
- **Edge** — a directed connection from one node's output port to another's input
  port.
- **Port** — a named connection point on a node. Default data port is `main`;
  branch ports include `true`/`false` (condition), case ports (switch), and
  `error`; agent sub-ports include `chat_model`/`memory`/`tool`/`output_parser`.
- **Item** — one element of the data array flowing on a connection
  (`{ json, binary? }`); nodes map over items. See
  [data & expressions](13-data-and-expressions.md).
- **Item pairing** — the link from an output item back to the input item that
  produced it.
- **Expression** — an `=`-prefixed config value evaluated against the run at
  execution time (pure, sandboxed).
- **Run** — one execution of a workflow. See
  [observability](16-observability-and-runs.md).
- **ExecutionStep** — the per-node record within a run (input/output/status/timing).
- **Capability trait** — a host-implemented interface (`LlmProvider`,
  `ToolInvoker`, `HttpClient`, `CodeRunner`, `StateStore`) through which nodes
  reach the outside world. See [capability traits](05-capability-traits.md).
- **Capabilities** — the bundle of capability implementations passed into a run.
- **Host** — the application embedding tinyflows (OpenHuman is the first).
- **Adapter seam** — the host module that implements the capability traits and
  wires tinyflows in (OpenHuman: `src/openhuman/tinyflows/`).
- **Compiler** — lowers a `WorkflowGraph` into a runnable
  `tinyagents::CompiledGraph`. See [execution engine](04-execution-engine.md).
- **Reducer** — folds each node's partial output into the shared graph state.
- **Super-step** — one scheduling round of the underlying graph engine; parallel
  nodes in the same super-step run concurrently.
- **Checkpoint** — a persisted snapshot of graph state at a super-step boundary,
  enabling durable resume and human-in-the-loop pauses.
- **HITL** — human-in-the-loop; a mid-run pause for user approval
  (`Interrupt` + `resume`).
- **Sub-workflow** — a workflow embedded and run as a node inside another.
- **tinyagents** — the underlying Rust state-graph execution engine tinyflows
  builds on.
- **Connection / `connection_ref`** — an opaque reference to a host-managed
  account/credential; secrets never live in the graph. See
  [credentials](15-credentials-and-connections.md).
