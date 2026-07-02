# Architecture

tinyflows takes a declarative workflow definition and runs it through a small,
fixed pipeline.

```
WorkflowGraph  →  validate  →  compile  →  engine::run
 (typed graph)   (structural)  (lowers)   (drives to completion,
                                           lowered onto tinyagents)
                                   ▲
              caps traits (LlmProvider / ToolInvoker / HttpClient /
              CodeRunner / StateStore) — host-injected, captured per run
```

1. **`WorkflowGraph`** — the serializable source of truth: typed `Node`s and
   `Edge`s. JSON is the wire format.
2. **`validate`** — structural checks (unique ids, exactly one trigger, edges
   reference existing nodes), run before compilation.
3. **`compile`** — validates and produces a `CompiledWorkflow`.
4. **`engine::run`** — lowers the compiled graph onto the
   [`tinyagents`](https://crates.io/crates/tinyagents) state-graph engine and
   drives it to completion, returning a `RunOutcome`.

## Per-run lowering

Lowering happens **per run**, inside `engine::run`. Each node becomes a
`tinyagents` handler that captures that run's host `Capabilities`, so the graph
built for one run carries exactly the caps handed to it. The engine wires:

- **Linear** paths (one successor per node).
- **Conditional branching** (successors on distinct ports; the taken port is
  recorded into state and routed on).
- **Parallel fan-out** (multiple successors sharing one port run concurrently via
  a `Command::goto`).
- **Fan-in barrier** (a node with more than one predecessor is wired with waiting
  edges so it runs only once all predecessors finish — the `merge` barrier).

## State layout

Run state is a single `serde_json::Value`:

```json
{
  "run":   { "trigger": { /* trigger payload */ } },
  "nodes": { "<id>": { "items": [ /* Item… */ ], "port": "true" } }
}
```

Data flowing on a connection is an **array of items**, not a single value. Each
`Item` is `{ json, binary?, paired_item? }`; a node maps its logic over its input
items and emits output items. A merge reducer folds each node's partial
`{ nodes: { id: { items } } }` update into the shared state — because every node
writes under its own id, independent updates never collide, which keeps parallel
fan-out correct. Field references use `=`-prefixed expressions.

## Host-agnostic seam

The crate never hard-codes an LLM, tool, HTTP, code, or persistence vendor.
Anything touching the outside world goes through a **capability trait** bundled in
`Capabilities` and injected by the host — see [Capability Traits](Capability-Traits).

## Deeper reading

- [`docs/01-architecture.md`](../blob/main/docs/01-architecture.md)
- [`docs/04-execution-engine.md`](../blob/main/docs/04-execution-engine.md)
