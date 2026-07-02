# 01 — Architecture

## The three-layer stack

```
┌─ Host application (e.g. OpenHuman, GPL v3) ─────────────────────────┐
│  • Persists WorkflowGraph definitions, CRUD, enable/disable          │
│  • Bridges triggers (schedule/webhook/app-event/…) → runs            │
│  • Implements the tinyflows capability traits with real services     │
│  • Owns the UI (visual canvas + agent-first chat authoring)          │
│         via an adapter seam (OpenHuman: src/openhuman/tinyflows/)     │
└───────────────────────────────┬─────────────────────────────────────┘
                                │ depends on (crates.io)
┌─ tinyflows crate (this repo, GPL-3.0) — HOST-AGNOSTIC ──────────────┐
│  model/     WorkflowGraph, Node, Edge, Port, NodeKind (serde)        │
│  validate   structural validation                                     │
│  compiler   WorkflowGraph → tinyagents::CompiledGraph<Value>         │
│  engine     drives the compiled graph, dispatches nodes by kind      │
│  nodes/     NodeExecutor trait + native & capability-backed nodes    │
│  caps/      capability traits (host injects impls) + mocks           │
│  error      ValidationError / EngineError                            │
└───────────────────────────────┬─────────────────────────────────────┘
                                │ depends on (crates.io)
┌─ tinyagents crate — StateGraph execution primitive ─────────────────┐
│  GraphBuilder, conditional/waiting edges, parallel super-steps,      │
│  reducers/channels, Interrupt/resume, Checkpointer, RecursionPolicy  │
└──────────────────────────────────────────────────────────────────────┘
```

## The host-agnostic rule (the most important design constraint)

tinyflows must **never** reference a specific vendor or the host's internals.
Concretely:

- The crate defines the **workflow model**, the **compiler** to tinyagents, and
  **pure native control-flow nodes** (if/switch/merge/split_out/transform).
- Everything that touches the outside world — LLM calls, integration tools, HTTP,
  code execution, durable state — is expressed as a **capability trait**
  ([`caps`](05-capability-traits.md)) that the host implements and injects via a
  [`Capabilities`](05-capability-traits.md) bundle.

This is what lets tinyflows be a genuine standalone open-source engine, keeps its
dependency graph tiny, and confines all host coupling to the adapter seam.

## Why build on tinyagents

`tinyagents` is a LangGraph-style durable state-graph engine already used in
OpenHuman (e.g. `model_council`, `agent_orchestration/workflow_runs`). It provides
exactly the primitives a workflow-automation engine needs — verified to support:

| Need | tinyagents primitive |
|------|----------------------|
| IF (true/false) | `add_conditional_edges` / `Command::with_goto` |
| Multi-way switch | N-way `add_conditional_edges(route_table)` |
| Merge / fan-in | `add_waiting_edge` barrier + `NamedBarrier` channel |
| Parallel branches | `with_parallel` + `with_max_concurrency` |
| Node-to-node data | typed state + reducers (we use `serde_json::Value`) |
| Loops | cycles + `RecursionPolicy` budget |
| Human-in-the-loop | `NodeResult::Interrupt` + `resume` + `Checkpointer` |
| Sub-workflows | subgraph nodes |

Building on it means tinyflows does **not** re-implement scheduling, parallelism,
durability, or resume — it lowers its declarative graph onto these primitives.
Details in [execution engine](04-execution-engine.md).

## Crate module map

| Module | Responsibility | Status |
|--------|----------------|--------|
| `model` | `WorkflowGraph`, `Node`, `Edge`, `Port`, `NodeKind`, `TriggerKind` | Implemented (A0) |
| `validate` | Structural validation (`validate(&WorkflowGraph)`) | Core checks done; extended in A1–A2 |
| `error` | `ValidationError`, `EngineError`, `Result` | Implemented |
| `caps` | Capability traits + `Capabilities` + mocks | Traits done; used in A3 |
| `nodes` | `NodeExecutor` trait, `NodeContext`, `NodeOutput`, per-kind nodes | Trait done; node logic A2–A3 |
| `compiler` | `compile(&WorkflowGraph) -> CompiledWorkflow` | Validates now; tinyagents lowering A1 |
| `engine` | `run(&CompiledWorkflow, input, caps)` | Entry point; driver A1/A3 |

See the [roadmap](08-roadmap.md) for what each stage delivers.
