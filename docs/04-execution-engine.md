# 04 — Execution engine

The engine turns a declarative [`WorkflowGraph`](02-workflow-model.md) into a
running computation by **lowering it onto `tinyagents`** and driving the result.

```
WorkflowGraph  ──validate──▶  compile()  ──▶  CompiledWorkflow
                                                   │
                                          engine::run(input, caps)
                                                   │
                                       tinyagents CompiledGraph<Value>
```

## Validation (`validate`)

Before anything runs, [`validate(&WorkflowGraph)`](../src/validate.rs) checks:
unique node ids, exactly one trigger node, and edges referencing existing nodes.
Cycle-legality and per-kind config validation are layered in during A1–A2. It
returns a [`ValidationError`](../src/error.rs).

## Compilation (`compile`)

`compile(&WorkflowGraph) -> Result<CompiledWorkflow>` validates and (from stage
A1) lowers the graph onto a `tinyagents::graph::CompiledGraph<serde_json::Value>`,
**built fresh per definition** — the same per-request build pattern OpenHuman uses
in `model_council/graph.rs`. Today it validates and returns an opaque handle;
the tinyagents lowering is the A1 deliverable.

### Why `serde_json::Value` as the graph state
tinyagents graphs are statically compiled with a single state type. To get the
**dynamic, untyped, per-node JSON I/O**, tinyflows uses `serde_json::Value` as the
graph state and a merge-tolerant reducer. This trades compile-time typing for the
flexibility a general workflow tool needs. (A future typed mode could specialize
the state per workflow, but is out of scope.)

### Lowering map (WorkflowGraph → tinyagents)

| Workflow construct | tinyagents primitive |
|--------------------|----------------------|
| Node | a graph node dispatched via [`NodeExecutor`](05-capability-traits.md) |
| Linear edge | `add_edge` |
| `condition` (true/false) | `add_conditional_edges` with a 2-entry route table |
| `switch` (N-way) | `add_conditional_edges` with an N-entry route table |
| `merge` (fan-in) | `add_waiting_edge` barrier + `NamedBarrier` channel |
| Parallel branches | `with_parallel` + `with_max_concurrency` |
| `split_out` (per-item) | `Send` / per-invocation args |
| Loops | cycles + `RecursionPolicy` budget |
| Human approval step | `NodeResult::Interrupt` + checkpoint `resume` |
| `sub_workflow` | a subgraph node (compile the child, embed it) |

## Running (`engine::run`)

`run(&CompiledWorkflow, input, &Capabilities)` seeds the run state from the
trigger `input`, then drives the compiled tinyagents graph. Each node is executed
by its [`NodeExecutor`](05-capability-traits.md) with a [`NodeContext`](../src/nodes/mod.rs)
`{ node, state, caps }`, and returns a [`NodeOutput`](../src/nodes/mod.rs)
`{ value, port }` — `value` is reduced into the run state; `port` selects the
outgoing branch for control-flow nodes. The final state is returned as
`RunOutcome`.

Today `run` returns `EngineError::Unimplemented`; the driver lands in A1
(minimal trigger→node path) and A3 (all node kinds).

## Durability, resume, and human-in-the-loop

tinyagents persists a checkpoint at each super-step boundary via a `Checkpointer`.
A node that needs user approval mid-run returns `NodeResult::Interrupt`; the run
pauses (checkpoint persisted) and later resumes with the user's decision via
`resume`. In OpenHuman the checkpointer is backed by SQLite
(`SqlRunLedgerCheckpointer`) — see [OpenHuman integration](09-openhuman-integration.md).

## Loops and budgets

Cycles are allowed (retry, iterate). tinyagents' `RecursionPolicy` bounds them
(`max_total_steps`, `max_visits_per_node`, `max_depth`) so a malformed loop can't
run forever. tinyflows sets sane budgets during compilation.

## Known engine constraints (from tinyagents)

- The compiled graph shape is **static** — node identities are fixed at compile
  time. tinyflows compiles a fresh graph per definition, so this is not a
  limitation in practice (workflow definitions are likewise statically authored).
- Parallel branches must not both overwrite the same scalar state key; tinyflows
  uses merge-tolerant reducers / barrier channels for fan-in (the `merge` node).
