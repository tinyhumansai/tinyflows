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
  "run":   {
    "trigger": { /* free-form payload that fired the run */ },
    "inputs":  { /* resolved declared inputs, one entry per declaration */ }
  },
  "nodes": { "<id>": { "items": [ /* Item… */ ], "port": "true" } }
}
```

Data flowing on a connection is an **array of items**, not a single value. Each
`Item` is `{ json, binary?, paired_item? }`; a node maps its logic over its input
items and emits output items. A merge reducer folds each node's partial
`{ nodes: { id: { items } } }` update into the shared state — because every node
writes under its own id, independent updates never collide, which keeps parallel
fan-out correct. Field references use `=`-prefixed expressions.

## Workflow inputs

A graph declares its parameters in a top-level `inputs` array — its public
signature, independent of how the workflow is triggered:

```json
{
  "name": "review-and-fix",
  "inputs": [
    { "name": "repo",  "type": "string", "required": true, "description": "Repo to review" },
    { "name": "depth", "type": "number", "default": 3 }
  ],
  "nodes": [ /* … */ ]
}
```

A caller supplies values through `engine::RunInput`, which carries them
alongside the trigger payload. They are validated against the declarations
**before** the run id is minted, the observer is notified, or the graph is
built — so an input error means provably nothing ran. Missing required values,
type mismatches, and undeclared keys are all rejected; declared inputs the
caller omits fall back to their `default`, or to `null` when optional.

Resolved values land at `run.inputs` and are lifted to the top-level `inputs`
expression scope, so node config reads them as `=inputs.repo`. Keep the two
channels distinct:

| | `run.trigger` | `inputs` |
|---|---|---|
| what it is | whatever fired the run | the workflow's declared parameters |
| shape | free-form | named, typed, validated |
| discoverable from the graph | no | yes |
| read as | `=run.trigger.<path>` | `=inputs.<name>` |

Inputs are **not** a secret channel — credentials reach a workflow through the
opaque connection reference the host resolves. A `sub_workflow` node forwards
values to its child with an `inputs` config object, each field resolved against
the parent's scope (`{"repo": "=inputs.repo"}`).

## Host-agnostic seam

The crate never hard-codes an LLM, tool, HTTP, code, or persistence vendor.
Anything touching the outside world goes through a **capability trait** — one of
the five bundled in `Capabilities` (`llm`, `tools`, `http`, `code`, `state`) and
injected by the host — see [Capability Traits](Capability-Traits).

## Deeper reading

- [Capability Traits](Capability-Traits) — the host-injected seam.
- [Node Catalog](Node-Catalog) — the node vocabulary the engine executes.
