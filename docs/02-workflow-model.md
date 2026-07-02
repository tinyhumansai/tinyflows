# 02 — Workflow model

The workflow model is the serializable source of truth for an automation. It
lives in the crate's `model` module and is a plain data structure with `serde`
round-tripping — so the same JSON is produced/consumed by the visual canvas, the
agent-first chat authoring flow, and the host's persistence layer.

## Types

### `WorkflowGraph`
The whole definition.

| Field | Type | Notes |
|-------|------|-------|
| `id` | `Option<String>` | Stable workflow id (assigned by the host on save) |
| `name` | `String` | Human-readable name |
| `nodes` | `Vec<Node>` | All nodes |
| `edges` | `Vec<Edge>` | Directed port-to-port connections |

Helpers: `trigger()` (the single trigger node, if exactly one), `node(id)`, and
`successors(id)` — a node's **direct** successors (immediate neighbors only, not
the transitive closure).

### `Node`
A single unit of work.

| Field | Type | Notes |
|-------|------|-------|
| `id` | `String` | Unique within the graph |
| `kind` | [`NodeKind`](03-node-catalog.md) | What the node does |
| `name` | `String` | Editor label |
| `config` | `serde_json::Value` | Kind-specific configuration (validated per kind at compile time) |
| `ports` | `Vec<Port>` | Declared output ports for branching/multi-output nodes |
| `position` | `Option<Position>` | Canvas coordinates; ignored by the engine |

Config is intentionally free-form JSON so the model stays stable as node kinds
grow. Each kind interprets/validates its own config (see the
[node catalog](03-node-catalog.md)).

### `Edge`
A directed connection from one node's output port to another's input port.

| Field | Type | Default |
|-------|------|---------|
| `from_node` | `String` | — |
| `from_port` | `String` | `"main"` |
| `to_node` | `String` | — |
| `to_port` | `String` | `"main"` |

Branching nodes emit on named ports (e.g. `"true"`/`"false"` for a condition, or
per-case ports for a switch); edges select which downstream path a port feeds.

### `Port`
`{ name: String, label: Option<String> }` — a named connection point. The default
data port is `"main"`. AI-agent sub-ports use names like `"chat_model"`,
`"memory"`, `"tool"`, `"output_parser"`.

### `Position`
`{ x: f64, y: f64 }` — canvas layout only; never affects execution.

### `NodeKind` / `TriggerKind`
Discriminators for what a node does and (for triggers) how it fires. Fully
enumerated in the [node catalog](03-node-catalog.md).

## JSON example

A minimal trigger → agent workflow:

```json
{
  "id": "wf_hello",
  "name": "Hello agent",
  "nodes": [
    { "id": "t", "kind": "trigger", "name": "On chat message",
      "config": { "trigger_kind": "chat_message" } },
    { "id": "a", "kind": "agent", "name": "Reply",
      "config": { "prompt": "Reply warmly to the user." } }
  ],
  "edges": [
    { "from_node": "t", "to_node": "a" }
  ]
}
```

Note `kind` serializes as `snake_case` (e.g. `"trigger"`, `"agent"`,
`"http_request"`, `"tool_call"`), and omitted `from_port`/`to_port` default to
`"main"`.

## Design notes

- **Dynamic JSON I/O.** At runtime, data flows as `serde_json::Value` (the graph
  state), matching the dynamic per-node I/O of visual automation tools rather than
  a rigid typed schema.
  See [execution engine](04-execution-engine.md).
- **Canvas-ready by construction.** Because the model is already a nodes+edges
  graph with `ports` and `position`, a visual editor is additive — it edits the
  same structure the agent-first flow produces.
- **Validation** happens before compilation — see
  [`validate`](04-execution-engine.md) and the [node catalog](03-node-catalog.md)
  for per-kind rules.
